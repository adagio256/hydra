use std::collections::BTreeSet;
use std::pin::Pin;
use std::time::Duration;
use std::time::Instant;

use serde::Deserialize;
use serde::Serialize;

use crate::AutoShutdown;
use crate::ChildSpec;
use crate::ChildType;
use crate::ExitReason;
use crate::GenServer;
use crate::GenServerOptions;
use crate::Message;
use crate::Pid;
use crate::Process;
use crate::ProcessFlags;
use crate::Reference;
use crate::Restart;
use crate::Shutdown;
use crate::SystemMessage;

/// A supervision child.
struct SupervisedChild {
    spec: ChildSpec,
    pid: Option<Pid>,
}

/// A supervisor message.
#[derive(Debug, Serialize, Deserialize)]
pub enum SupervisorMessage {
    TryAgainRestartPid(Pid),
    TryAgainRestartId(String),
}

/// The supervision strategy to use for each child.
#[derive(Debug, Clone, Copy)]
pub enum SupervisionStrategy {
    /// If a child process terminates, only that process is restarted.
    OneForOne,
    /// If a child process terminates, all other child processes are terminated and then all child processes are restarted.
    OneForAll,
    /// If a child process terminates, the terminated child process and the rest of the children started after it, are terminated and restarted.
    RestForOne,
}

/// A supervisor is a process which supervises other processes, which we refer to as child processes.
/// Supervisors are used to build a hierarchical process structure called a supervision tree.
/// Supervision trees provide fault-tolerance and encapsulate how our applications start and shutdown.
pub struct Supervisor {
    children: Vec<SupervisedChild>,
    identifiers: BTreeSet<String>,
    restarts: Vec<Instant>,
    strategy: SupervisionStrategy,
    auto_shutdown: AutoShutdown,
    max_restarts: usize,
    max_duration: Duration,
}

impl Supervisor {
    /// Constructs a new instance of [Supervisor] with no children.
    pub const fn new() -> Self {
        Self {
            children: Vec::new(),
            identifiers: BTreeSet::new(),
            restarts: Vec::new(),
            strategy: SupervisionStrategy::OneForOne,
            auto_shutdown: AutoShutdown::Never,
            max_restarts: 3,
            max_duration: Duration::from_secs(5),
        }
    }

    /// Constructs a new instance of [Supervisor] with the given children.
    pub fn with_children<T: IntoIterator<Item = ChildSpec>>(children: T) -> Self {
        let mut result = Self::new();

        for child in children {
            result = result.add_child(child);
        }

        result
    }

    /// Adds a child to this [Supervisor].
    pub fn add_child(mut self, child: ChildSpec) -> Self {
        if self.identifiers.contains(&child.id) {
            panic!("Child id was not unique!");
        }

        self.identifiers.insert(child.id.clone());

        self.children.push(SupervisedChild {
            spec: child,
            pid: None,
        });

        self
    }

    /// Sets the supervision strategy for the [Supervisor].
    pub const fn strategy(mut self, strategy: SupervisionStrategy) -> Self {
        self.strategy = strategy;
        self
    }

    /// Sets the behavior to use when a significant process exits.
    pub const fn auto_shutdown(mut self, auto_shutdown: AutoShutdown) -> Self {
        self.auto_shutdown = auto_shutdown;
        self
    }

    /// Sets the maximum number of restarts allowed in a time frame.
    ///
    /// Defaults to 3.
    pub const fn max_restarts(mut self, max_restarts: usize) -> Self {
        self.max_restarts = max_restarts;
        self
    }

    /// Sets the time frame in which `max_restarts` applies.
    ///
    /// Defaults to 5s.
    pub const fn max_duration(mut self, max_duration: Duration) -> Self {
        self.max_duration = max_duration;
        self
    }

    pub async fn start_link(self, options: GenServerOptions) -> Result<Pid, ExitReason> {
        GenServer::start_link(self, (), options).await
    }

    /// Starts all of the children.
    async fn start_children(&mut self) -> Result<(), ExitReason> {
        let mut remove: Vec<usize> = Vec::new();

        for index in 0..self.children.len() {
            match self.start_child(index).await {
                Ok(pid) => {
                    let child = &mut self.children[index];

                    child.pid = pid;

                    if child.is_temporary() && pid.is_none() {
                        remove.push(index);
                    }
                }
                Err(reason) => {
                    #[cfg(feature = "tracing")]
                    tracing::error!(reason = ?reason, child_id = ?self.children[index].spec.id, "Start error.");

                    return Err(ExitReason::from("failed_to_start_child"));
                }
            }
        }

        for index in remove.into_iter().rev() {
            self.remove_child(index);
        }

        Ok(())
    }

    /// Terminates all of the children.
    async fn terminate_children(&mut self) {
        let mut remove: Vec<usize> = Vec::new();

        for (index, child) in self.children.iter_mut().enumerate().rev() {
            if child.is_temporary() {
                remove.push(index);
            }

            let Some(pid) = child.pid.take() else {
                continue;
            };

            if let Err(reason) = shutdown(pid, child.shutdown()).await {
                #[cfg(feature = "tracing")]
                tracing::error!(reason = ?reason, child_pid = ?pid, "Shutdown error.");

                #[cfg(not(feature = "tracing"))]
                let _ = reason;
            }
        }

        for index in remove {
            self.remove_child(index);
        }
    }

    /// Checks all of the children for correct specification and then starts them.
    async fn init_children(&mut self) -> Result<(), ExitReason> {
        if let Err(reason) = self.start_children().await {
            self.terminate_children().await;

            return Err(reason);
        }

        Ok(())
    }

    /// Restarts a child that exited for the given `reason`.
    async fn restart_child(&mut self, pid: Pid, reason: ExitReason) -> Result<(), ExitReason> {
        let Some(index) = self.find_child(pid) else {
            return Ok(());
        };

        let child = &mut self.children[index];

        // Permanent children are always restarted.
        if child.is_permanent() {
            #[cfg(feature = "tracing")]
            tracing::error!(reason = ?reason, child_id = ?child.spec.id, child_pid = ?child.pid, "Child terminated.");

            if self.add_restart() {
                return Err(ExitReason::from("shutdown"));
            }

            self.restart(index).await;

            return Ok(());
        }

        // If it's not permanent, check if it's a normal reason.
        if reason.is_normal() || reason == "shutdown" {
            let child = self.remove_child(index);

            if self.check_auto_shutdown(child) {
                return Err(ExitReason::from("shutdown"));
            } else {
                return Ok(());
            }
        }

        // Not a normal reason, check if transient.
        if child.is_transient() {
            #[cfg(feature = "tracing")]
            tracing::error!(reason = ?reason, child_id = ?child.spec.id, child_pid = ?child.pid, "Child terminated.");

            if self.add_restart() {
                return Err(ExitReason::from("shutdown"));
            }

            self.restart(index).await;

            return Ok(());
        }

        // Not transient, check if temporary and clean up.
        if child.is_temporary() {
            #[cfg(feature = "tracing")]
            tracing::error!(reason = ?reason, child_id = ?child.spec.id, child_pid = ?child.pid, "Child terminated.");

            let child = self.remove_child(index);

            if self.check_auto_shutdown(child) {
                return Err(ExitReason::from("shutdown"));
            }
        }

        Ok(())
    }

    /// Restarts one or more children starting with the given `index` based on the current strategy.
    async fn restart(&mut self, index: usize) {
        match self.strategy {
            SupervisionStrategy::OneForOne => {
                match self.start_child(index).await {
                    Ok(pid) => {
                        self.children[index].pid = pid;
                    }
                    Err(reason) => {
                        let id = self.children[index].id();

                        #[cfg(feature = "tracing")]
                        tracing::error!(reason = ?reason, child_id = ?id, child_pid = ?self.children[index].pid, "Start error.");

                        Supervisor::cast(
                            Process::current(),
                            SupervisorMessage::TryAgainRestartId(id),
                        );
                    }
                };
            }
            SupervisionStrategy::RestForOne => {
                //
            }
            _ => unimplemented!(),
        }
    }

    /// Starts the given child by it's index and returns what the result was.
    async fn start_child(&mut self, index: usize) -> Result<Option<Pid>, ExitReason> {
        let child = &mut self.children[index];
        let start_child = Pin::from(child.spec.start.as_ref().unwrap()()).await;

        match start_child {
            Ok(pid) => {
                #[cfg(feature = "tracing")]
                tracing::info!(child_id = ?child.spec.id, child_pid = ?pid, "Started child.");

                Ok(Some(pid))
            }
            Err(reason) => {
                if reason.is_ignore() {
                    #[cfg(feature = "tracing")]
                    tracing::info!(child_id = ?child.spec.id, child_pid = ?None::<Pid>, "Started child.");

                    Ok(None)
                } else {
                    Err(reason)
                }
            }
        }
    }

    /// Checks whether or not we should automatically shutdown the supervisor. Returns `true` if so.
    fn check_auto_shutdown(&mut self, child: SupervisedChild) -> bool {
        if matches!(self.auto_shutdown, AutoShutdown::Never) {
            return false;
        }

        if !child.spec.significant {
            return false;
        }

        if matches!(self.auto_shutdown, AutoShutdown::AnySignificant) {
            return true;
        }

        self.children.iter().any(|child| {
            if child.pid.is_none() {
                return false;
            }

            child.spec.significant
        })
    }

    /// Adds another restart to the backlog and returns `true` if we've exceeded our quota of restarts.
    fn add_restart(&mut self) -> bool {
        let now = Instant::now();
        let threshold = now - self.max_duration;

        self.restarts.retain(|restart| *restart >= threshold);
        self.restarts.push(now);

        if self.restarts.len() > self.max_restarts {
            #[cfg(feature = "tracing")]
            tracing::error!(restarts = ?self.restarts, "Reached max restart intensity.");

            return true;
        }

        false
    }

    /// Removes a child from the supervisor.
    fn remove_child(&mut self, index: usize) -> SupervisedChild {
        let child = self.children.remove(index);

        self.identifiers.remove(&child.spec.id);

        child
    }

    /// Finds a child by the given `pid`.
    fn find_child(&mut self, pid: Pid) -> Option<usize> {
        self.children
            .iter()
            .position(|child| child.pid.is_some_and(|cpid| cpid == pid))
    }
}

impl SupervisedChild {
    /// Returns `true` if the child is a permanent process.
    pub const fn is_permanent(&self) -> bool {
        matches!(self.spec.restart, Restart::Permanent)
    }

    /// Returns `true` if the child is a transient process.
    pub const fn is_transient(&self) -> bool {
        matches!(self.spec.restart, Restart::Transient)
    }

    /// Returns `true` if the child is a temporary process.
    pub const fn is_temporary(&self) -> bool {
        matches!(self.spec.restart, Restart::Temporary)
    }

    /// Returns the unique id of the child.
    pub fn id(&self) -> String {
        self.spec.id.clone()
    }

    /// Returns how the child should be terminated.
    pub const fn shutdown(&self) -> Shutdown {
        match self.spec.shutdown {
            None => match self.spec.child_type {
                ChildType::Worker => Shutdown::Duration(Duration::from_secs(5)),
                ChildType::Supervisor => Shutdown::Infinity,
            },
            Some(shutdown) => shutdown,
        }
    }
}

impl GenServer for Supervisor {
    type InitArg = ();
    type Message = SupervisorMessage;

    async fn init(&mut self, _: Self::InitArg) -> Result<(), ExitReason> {
        Process::set_flags(ProcessFlags::TRAP_EXIT);

        self.init_children().await
    }

    async fn handle_cast(&mut self, message: Self::Message) -> Result<(), ExitReason> {
        match message {
            SupervisorMessage::TryAgainRestartPid(pid) => {
                //
            }
            _ => unreachable!(),
        }

        Ok(())
    }

    async fn handle_info(&mut self, info: Message<Self::Message>) -> Result<(), ExitReason> {
        match info {
            Message::System(SystemMessage::Exit(pid, reason)) => {
                self.restart_child(pid, reason).await
            }
            _ => Ok(()),
        }
    }
}

/// Terminates the given `pid` using the given `shutdown` method.
async fn shutdown(pid: Pid, shutdown: Shutdown) -> Result<(), ExitReason> {
    let monitor = Process::monitor(pid);

    match shutdown {
        Shutdown::BrutalKill => shutdown_brutal_kill(pid, monitor).await,
        Shutdown::Duration(timeout) => shutdown_timeout(pid, monitor, timeout).await,
        Shutdown::Infinity => shutdown_infinity(pid, monitor).await,
    }
}

/// Terminates the given `pid` by forcefully killing it and waiting for the `monitor` to fire.
async fn shutdown_brutal_kill(pid: Pid, monitor: Reference) -> Result<(), ExitReason> {
    Process::exit(pid, ExitReason::Kill);

    let result = Process::receiver()
        .ignore_type()
        .select::<(), _>(|message| {
            match message {
                Message::System(SystemMessage::ProcessDown(_, tag, _)) => {
                    // Make sure that the tag matches.
                    *tag == monitor
                }
                _ => false,
            }
        })
        .await;

    let Message::System(SystemMessage::ProcessDown(_, _, reason)) = result else {
        unreachable!()
    };

    unlink_flush(pid, reason);

    Ok(())
}

/// Terminates the given `pid` by gracefully waiting for `timeout`
/// then forcefully kills it as necessary while waiting for `monitor` to fire.
async fn shutdown_timeout(
    pid: Pid,
    monitor: Reference,
    timeout: Duration,
) -> Result<(), ExitReason> {
    Process::exit(pid, ExitReason::from("shutdown"));

    let receiver = Process::receiver()
        .ignore_type()
        .select::<(), _>(|message| {
            match message {
                Message::System(SystemMessage::ProcessDown(_, tag, _)) => {
                    // Make sure that the tag matches.
                    *tag == monitor
                }
                _ => false,
            }
        });

    let result = Process::timeout(timeout, receiver).await;

    match result {
        Ok(Message::System(SystemMessage::ProcessDown(_, _, reason))) => {
            unlink_flush(pid, reason);

            Ok(())
        }
        Ok(_) => unreachable!(),
        Err(_) => shutdown_brutal_kill(pid, monitor).await,
    }
}

/// Terminates the given `pid` by gracefully waiting indefinitely for the `monitor` to fire.
async fn shutdown_infinity(pid: Pid, monitor: Reference) -> Result<(), ExitReason> {
    Process::exit(pid, ExitReason::from("shutdown"));

    let result = Process::receiver()
        .ignore_type()
        .select::<(), _>(|message| {
            match message {
                Message::System(SystemMessage::ProcessDown(_, tag, _)) => {
                    // Make sure that the tag matches.
                    *tag == monitor
                }
                _ => false,
            }
        })
        .await;

    let Message::System(SystemMessage::ProcessDown(_, _, reason)) = result else {
        unreachable!()
    };

    unlink_flush(pid, reason);

    Ok(())
}

/// Unlinks the given process and ensures that any pending exit signal is flushed from the message queue.
///
/// Returns the real [ExitReason] or the `default_reason` if no signal was found.
fn unlink_flush(pid: Pid, default_reason: ExitReason) -> ExitReason {
    Process::unlink(pid);

    let mut reason = default_reason;

    Process::receiver()
        .ignore_type()
        .drop::<(), _>(|message| match message {
            Message::System(SystemMessage::Exit(epid, ereason)) => {
                if *epid == pid {
                    reason = ereason.clone();
                    return true;
                }

                false
            }
            _ => false,
        });

    reason
}
