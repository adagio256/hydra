use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;

use tokio::net::TcpStream;

use tokio_util::codec::Framed;

use pingora_timeout::timeout;

use futures_util::stream::SplitSink;
use futures_util::stream::SplitStream;
use futures_util::SinkExt;
use futures_util::StreamExt;

use crate::frame::Codec;
use crate::frame::Frame;
use crate::frame::Hello;
use crate::frame::Ping;

use crate::node_accept;
use crate::node_set_send_recv;
use crate::Local;
use crate::Message;
use crate::Node;
use crate::NodeLocalSupervisor;
use crate::Pid;
use crate::Process;

type Reader = SplitStream<Framed<TcpStream, Codec>>;
type Writer = SplitSink<Framed<TcpStream, Codec>, Frame>;

#[derive(Serialize, Deserialize)]
enum NodeRemoteSenderMessage {
    SendFrame(Local<Frame>),
}

#[derive(Serialize, Deserialize, Clone, Copy)]
enum NodeRemoteSupervisorMessage {
    SendPong,
}

struct NodeRemoteSupervisor {
    node: Node,
    process: Pid,
    local_supervisor: Arc<NodeLocalSupervisor>,
}

impl Drop for NodeRemoteSupervisor {
    fn drop(&mut self) {
        // We need to clean up this node!
        let _ = self.node;

        unimplemented!()
    }
}

async fn node_remote_sender(mut writer: Writer, supervisor: Arc<NodeRemoteSupervisor>) {
    let send_timeout = supervisor.local_supervisor.options.heartbeat_interval;

    loop {
        let Ok(message) =
            timeout(send_timeout, Process::receive::<NodeRemoteSenderMessage>()).await
        else {
            writer
                .send(Ping.into())
                .await
                .expect("Failed to send a message to the remote node!");
            continue;
        };

        match message {
            Message::User(_) => {
                //
            }
            _ => unreachable!(),
        }
    }
}

async fn node_remote_receiver(mut reader: Reader, supervisor: Arc<NodeRemoteSupervisor>) {
    let recv_timeout = supervisor.local_supervisor.options.heartbeat_timeout;

    loop {
        let message = timeout(recv_timeout, reader.next())
            .await
            .expect("Remote node timed out!")
            .unwrap()
            .expect("Failed to receive a message from the remote node!");

        match message {
            Frame::Hello(_) => unreachable!("Should never receive hello frame!"),
            Frame::Ping => {
                Process::send(supervisor.process, NodeRemoteSupervisorMessage::SendPong);
            }
            Frame::Pong => {
                // Maybe log this in metrics somewhere!
            }
        }
    }
}

async fn node_remote_supervisor(
    writer: Writer,
    reader: Reader,
    hello: Hello,
    supervisor: Arc<NodeLocalSupervisor>,
) {
    let node: Node = (hello.name, hello.broadcast_address).into();

    if !node_accept(node.clone(), Process::current()) {
        panic!("Not accepting node supervisor!");
    }

    let supervisor: Arc<NodeRemoteSupervisor> = Arc::new(NodeRemoteSupervisor {
        node: node.clone(),
        process: Process::current(),
        local_supervisor: supervisor,
    });

    Process::link(supervisor.local_supervisor.process);

    let sender = Process::spawn_link(node_remote_sender(writer, supervisor.clone()));
    let receiver = Process::spawn_link(node_remote_receiver(reader, supervisor.clone()));

    node_set_send_recv(node, sender, receiver);

    loop {
        let message = Process::receive::<NodeRemoteSupervisorMessage>().await;

        match message {
            Message::User(NodeRemoteSupervisorMessage::SendPong) => {
                // TODO: Send to the sender about a pong message.
                unimplemented!()
            }
            _ => unreachable!(),
        }
    }
}

pub async fn node_remote_accepter(socket: TcpStream, supervisor: Arc<NodeLocalSupervisor>) {
    let framed = Framed::new(socket, Codec::new());
    let (mut writer, mut reader) = framed.split();

    let hello = Hello::new(
        supervisor.name.clone(),
        supervisor.options.broadcast_address,
    );

    let handshake_timeout = supervisor.options.handshake_timeout;

    timeout(handshake_timeout, writer.send(hello.into()))
        .await
        .expect("Timed out while sending hello handshake packet!")
        .expect("Failed to send hello handshake packet!");

    let frame = timeout(handshake_timeout, reader.next())
        .await
        .expect("Timed out while receiving hello handshake packet!")
        .unwrap()
        .expect("Failed to receive hello handshake packet!");

    if let Frame::Hello(mut hello) = frame {
        if hello.validate() {
            Process::spawn(node_remote_supervisor(writer, reader, hello, supervisor));
        } else {
            panic!("Node handshake failed validation!");
        }
    }

    panic!("Received incorrect frame for node handshake!");
}
