/*
 * Copyright 2018 Intel Corporation
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 * ------------------------------------------------------------------------------
 */

use protobuf::{Message as ProtobufMessage, ProtobufError, RepeatedField};
use rand::{distributions::Alphanumeric, Rng};

use crate::consensus::engine::*;
use crate::consensus::zmq_service::ZmqService;

use crate::messaging::stream::MessageConnection;
use crate::messaging::stream::MessageSender;
use crate::messaging::stream::ReceiveError;
use crate::messaging::stream::SendError;
use crate::messaging::zmq_stream::{ZmqMessageConnection, ZmqMessageSender};

use crate::messages::consensus::*;
use crate::messages::network::PingResponse;
use crate::messages::validator::{Message, Message_MessageType};

use std::sync::mpsc::{self, channel, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

const REGISTER_TIMEOUT: u64 = 300;
const SERVICE_TIMEOUT: u64 = 300;
const INITAL_RETRY_DELAY: Duration = Duration::from_millis(100);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(3);

/// Generates a random correlation id for use in Message
fn generate_correlation_id() -> String {
    const LENGTH: usize = 16;
    let mut rng = rand::thread_rng();
    [0..LENGTH]
        .iter()
        .map(|_| rng.sample(Alphanumeric))
        .map(char::from)
        .collect::<String>()
}

pub struct ZmqDriver {
    stop_receiver: Receiver<()>,
}

impl ZmqDriver {
    /// Create a new ZMQ-based Consensus Engine driver and a handle for stopping it
    pub fn new() -> (Self, Stop) {
        let (stop_sender, stop_receiver) = channel();
        let stop = Stop {
            sender: stop_sender,
        };
        let driver = ZmqDriver { stop_receiver };
        (driver, stop)
    }

    /// Start the driver with the given engine, consuming both
    ///
    /// The engine's start method will be run from the current thread and this method should block
    /// until the engine shutsdown.
    pub fn start<T: AsRef<str>, E: Engine>(self, endpoint: T, mut engine: E) -> Result<(), Error> {
        let validator_connection = ZmqMessageConnection::new(endpoint.as_ref());
        let (mut validator_sender, validator_receiver) = validator_connection.create();

        let validator_sender_clone = validator_sender.clone();
        let (update_sender, update_receiver) = channel();

        // Validators version 1.1 send startup info with the registration response; newer versions
        // will send an activation message with the startup info
        let startup_state = match register(
            &mut validator_sender,
            Duration::from_secs(REGISTER_TIMEOUT),
            engine.name(),
            engine.version(),
            engine.additional_protocols(),
        )? {
            Some(state) => state,
            None => wait_until_active(&validator_sender, &validator_receiver)?,
        };

        let driver_thread = thread::spawn(move || {
            driver_loop(
                update_sender,
                &self.stop_receiver,
                validator_sender,
                &validator_receiver,
            )
        });

        engine.start(
            update_receiver,
            Box::new(ZmqService::new(
                validator_sender_clone,
                Duration::from_secs(SERVICE_TIMEOUT),
            )),
            startup_state,
        )?;

        driver_thread.join().expect("Driver panicked")
    }
}

/// Utility class for signaling that the driver should be shutdown
#[derive(Clone)]
pub struct Stop {
    sender: Sender<()>,
}

impl Stop {
    pub fn stop(&self) {
        self.sender
            .send(())
            .unwrap_or_else(|err| error!("Failed to send stop signal: {:?}", err));
    }
}

fn driver_loop(
    mut update_sender: Sender<Update>,
    stop_receiver: &Receiver<()>,
    mut validator_sender: ZmqMessageSender,
    validator_receiver: &Receiver<Result<Message, ReceiveError>>,
) -> Result<(), Error> {
    loop {
        match validator_receiver.recv_timeout(Duration::from_millis(100)) {
            Err(RecvTimeoutError::Timeout) => {
                if stop_receiver.try_recv().is_ok() {
                    update_sender.send(Update::Shutdown)?;
                    break Ok(());
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                break Err(Error::ReceiveError("Sender disconnected".into()));
            }
            Ok(Err(err)) => {
                break Err(Error::ReceiveError(format!(
                    "Unexpected error while receiving: {}",
                    err
                )));
            }
            Ok(Ok(msg)) if msg.get_message_type() == Message_MessageType::PING_REQUEST => {
                send_ping_reply(&mut validator_sender, msg.get_correlation_id())?;
            }
            Ok(Ok(msg)) => {
                if let Err(err) = handle_update(&msg, &mut validator_sender, &mut update_sender) {
                    break Err(err);
                }
                if stop_receiver.try_recv().is_ok() {
                    update_sender.send(Update::Shutdown)?;
                    break Ok(());
                }
            }
        }
    }
}

pub fn register(
    sender: &mut dyn MessageSender,
    timeout: Duration,
    name: String,
    version: String,
    additional_protocols: Vec<(String, String)>,
) -> Result<Option<StartupState>, Error> {
    let mut request = ConsensusRegisterRequest::new();
    request.set_name(name);
    request.set_version(version);
    request.set_additional_protocols(RepeatedField::from(protocols_from_tuples(
        additional_protocols,
    )));
    let request = request.write_to_bytes()?;

    let mut msg = sender
        .send(
            Message_MessageType::CONSENSUS_REGISTER_REQUEST,
            &generate_correlation_id(),
            &request,
        )?
        .get_timeout(timeout)?;

    let ret: Result<Option<StartupState>, Error>;

    // Keep trying to register until the response is something other
    // than NOT_READY.

    let mut retry_delay = INITAL_RETRY_DELAY;
    loop {
        match msg.get_message_type() {
            Message_MessageType::CONSENSUS_REGISTER_RESPONSE => {
                let mut response: ConsensusRegisterResponse =
                    ProtobufMessage::parse_from_bytes(msg.get_content())?;

                match response.get_status() {
                    ConsensusRegisterResponse_Status::OK => {
                        ret = if response.chain_head.is_some() && response.local_peer_info.is_some()
                        {
                            Ok(Some(StartupState {
                                chain_head: response.take_chain_head().into(),
                                peers: response
                                    .take_peers()
                                    .into_iter()
                                    .map(|info| info.into())
                                    .collect(),
                                local_peer_info: response.take_local_peer_info().into(),
                            }))
                        } else {
                            Ok(None)
                        };

                        break;
                    }
                    ConsensusRegisterResponse_Status::NOT_READY => {
                        thread::sleep(retry_delay);
                        if retry_delay < MAX_RETRY_DELAY {
                            retry_delay *= 2;
                            if retry_delay > MAX_RETRY_DELAY {
                                retry_delay = MAX_RETRY_DELAY;
                            }
                        }
                        msg = sender
                            .send(
                                Message_MessageType::CONSENSUS_REGISTER_REQUEST,
                                &generate_correlation_id(),
                                &request,
                            )?
                            .get_timeout(timeout)?;

                        continue;
                    }
                    status => {
                        ret = Err(Error::ReceiveError(format!(
                            "Registration failed with status {:?}",
                            status
                        )));

                        break;
                    }
                };
            }
            unexpected => {
                ret = Err(Error::ReceiveError(format!(
                    "Received unexpected message type: {:?}",
                    unexpected
                )));

                break;
            }
        }
    }

    ret
}

fn wait_until_active(
    validator_sender: &ZmqMessageSender,
    validator_receiver: &Receiver<Result<Message, ReceiveError>>,
) -> Result<StartupState, Error> {
    use self::Message_MessageType::*;

    let ret: Result<StartupState, Error>;

    loop {
        match validator_receiver.recv_timeout(Duration::from_millis(100)) {
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                ret = Err(Error::ReceiveError("Sender disconnected".into()));
                break;
            }
            Ok(Err(err)) => {
                ret = Err(Error::ReceiveError(format!(
                    "Unexpected error while receiving: {}",
                    err
                )));
                break;
            }
            Ok(Ok(msg)) => {
                if let CONSENSUS_NOTIFY_ENGINE_ACTIVATED = msg.get_message_type() {
                    let mut content: ConsensusNotifyEngineActivated =
                        ProtobufMessage::parse_from_bytes(msg.get_content())?;

                    ret = Ok(StartupState {
                        chain_head: content.take_chain_head().into(),
                        peers: content
                            .take_peers()
                            .into_iter()
                            .map(|info| info.into())
                            .collect(),
                        local_peer_info: content.take_local_peer_info().into(),
                    });

                    validator_sender.reply(
                        Message_MessageType::CONSENSUS_NOTIFY_ACK,
                        msg.get_correlation_id(),
                        &[],
                    )?;

                    break;
                }
            }
        }
    }

    ret
}

fn handle_update(
    msg: &Message,
    validator_sender: &mut dyn MessageSender,
    update_sender: &mut Sender<Update>,
) -> Result<(), Error> {
    use self::Message_MessageType::*;

    let update = match msg.get_message_type() {
        CONSENSUS_NOTIFY_PEER_CONNECTED => {
            let mut request: ConsensusNotifyPeerConnected =
                ProtobufMessage::parse_from_bytes(msg.get_content())?;
            Update::PeerConnected(request.take_peer_info().into())
        }
        CONSENSUS_NOTIFY_PEER_DISCONNECTED => {
            let mut request: ConsensusNotifyPeerDisconnected =
                ProtobufMessage::parse_from_bytes(msg.get_content())?;
            Update::PeerDisconnected(request.take_peer_id())
        }
        CONSENSUS_NOTIFY_PEER_MESSAGE => {
            let mut request: ConsensusNotifyPeerMessage =
                ProtobufMessage::parse_from_bytes(msg.get_content())?;
            let header: ConsensusPeerMessageHeader =
                ProtobufMessage::parse_from_bytes(request.get_message().get_header())?;
            let message = request.take_message();
            Update::PeerMessage(
                from_consensus_peer_message(message, header),
                request.take_sender_id(),
            )
        }
        CONSENSUS_NOTIFY_BLOCK_NEW => {
            let mut request: ConsensusNotifyBlockNew =
                ProtobufMessage::parse_from_bytes(msg.get_content())?;
            Update::BlockNew(request.take_block().into())
        }
        CONSENSUS_NOTIFY_BLOCK_VALID => {
            let mut request: ConsensusNotifyBlockValid =
                ProtobufMessage::parse_from_bytes(msg.get_content())?;
            Update::BlockValid(request.take_block_id())
        }
        CONSENSUS_NOTIFY_BLOCK_INVALID => {
            let mut request: ConsensusNotifyBlockInvalid =
                ProtobufMessage::parse_from_bytes(msg.get_content())?;
            Update::BlockInvalid(request.take_block_id())
        }
        CONSENSUS_NOTIFY_BLOCK_COMMIT => {
            let mut request: ConsensusNotifyBlockCommit =
                ProtobufMessage::parse_from_bytes(msg.get_content())?;
            Update::BlockCommit(request.take_block_id())
        }
        CONSENSUS_NOTIFY_ENGINE_DEACTIVATED => Update::Shutdown,
        unexpected => {
            warn!(
                "Received unexpected message type: {:?}; ignoring",
                unexpected
            );
            return Ok(());
        }
    };

    update_sender.send(update)?;
    validator_sender.reply(
        Message_MessageType::CONSENSUS_NOTIFY_ACK,
        msg.get_correlation_id(),
        &[],
    )?;
    Ok(())
}

fn send_ping_reply(
    validator_sender: &mut dyn MessageSender,
    correlation_id: &str,
) -> Result<(), Error> {
    trace!("sending PingResponse");
    validator_sender.reply(
        Message_MessageType::PING_RESPONSE,
        correlation_id,
        &PingResponse::new().write_to_bytes()?,
    )?;
    Ok(())
}

fn protocols_from_tuples(
    protocols: Vec<(String, String)>,
) -> Vec<ConsensusRegisterRequest_Protocol> {
    protocols
        .iter()
        .map(|(p_name, p_version)| {
            let mut protocol = ConsensusRegisterRequest_Protocol::new();
            protocol.set_name(p_name.to_string());
            protocol.set_version(p_version.to_string());
            protocol
        })
        .collect::<Vec<_>>()
}

impl From<ConsensusBlock> for Block {
    fn from(mut c_block: ConsensusBlock) -> Block {
        Block {
            block_id: c_block.take_block_id(),
            previous_id: c_block.take_previous_id(),
            signer_id: c_block.take_signer_id(),
            block_num: c_block.get_block_num(),
            payload: c_block.take_payload(),
            summary: c_block.take_summary(),
        }
    }
}

impl From<ConsensusPeerInfo> for PeerInfo {
    fn from(mut c_peer_info: ConsensusPeerInfo) -> PeerInfo {
        PeerInfo {
            peer_id: c_peer_info.take_peer_id(),
        }
    }
}

fn from_consensus_peer_message(
    mut c_msg: ConsensusPeerMessage,
    mut c_msg_header: ConsensusPeerMessageHeader,
) -> PeerMessage {
    PeerMessage {
        header: PeerMessageHeader {
            signer_id: c_msg_header.take_signer_id(),
            content_sha512: c_msg_header.take_content_sha512(),
            message_type: c_msg_header.take_message_type(),
            name: c_msg_header.take_name(),
            version: c_msg_header.take_version(),
        },
        header_bytes: c_msg.take_header(),
        header_signature: c_msg.take_header_signature(),
        content: c_msg.take_content(),
    }
}

impl From<ProtobufError> for Error {
    fn from(error: ProtobufError) -> Error {
        use self::ProtobufError::*;
        match error {
            IoError(err) => Error::EncodingError(format!("{}", err)),
            WireError(err) => Error::EncodingError(format!("{:?}", err)),
            Utf8(err) => Error::EncodingError(format!("{}", err)),
            MessageNotInitialized { message: err } => Error::EncodingError(err.to_string()),
        }
    }
}

impl From<SendError> for Error {
    fn from(error: SendError) -> Error {
        Error::SendError(format!("{}", error))
    }
}

impl From<mpsc::SendError<Update>> for Error {
    fn from(error: mpsc::SendError<Update>) -> Error {
        Error::SendError(format!("{}", error))
    }
}

impl From<ReceiveError> for Error {
    fn from(error: ReceiveError) -> Error {
        Error::ReceiveError(format!("{}", error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::consensus::engine::tests::MockEngine;
    use crate::messages::network::PingRequest;
    use std::sync::{Arc, Mutex};
    use zmq;

    fn send_req_rep<I: protobuf::Message, O: protobuf::Message>(
        connection_id: &[u8],
        socket: &zmq::Socket,
        request: I,
        request_type: Message_MessageType,
        response_type: Message_MessageType,
    ) -> O {
        let correlation_id = generate_correlation_id();
        let mut msg = Message::new();
        msg.set_message_type(request_type);
        msg.set_correlation_id(correlation_id.clone());
        msg.set_content(request.write_to_bytes().unwrap());
        socket
            .send_multipart(&[connection_id, &msg.write_to_bytes().unwrap()], 0)
            .unwrap();
        let msg: Message =
            ProtobufMessage::parse_from_bytes(&socket.recv_multipart(0).unwrap()[1]).unwrap();
        assert!(msg.get_message_type() == response_type);
        ProtobufMessage::parse_from_bytes(&msg.get_content()).unwrap()
    }

    fn recv_rep<I: protobuf::Message, O: protobuf::Message>(
        socket: &zmq::Socket,
        request_type: Message_MessageType,
        response: I,
        response_type: Message_MessageType,
    ) -> (Vec<u8>, O) {
        let mut parts = socket.recv_multipart(0).unwrap();
        assert!(parts.len() == 2);

        let mut msg: Message = ProtobufMessage::parse_from_bytes(&parts.pop().unwrap()).unwrap();
        let connection_id = parts.pop().unwrap();
        assert!(msg.get_message_type() == request_type);
        let request: O = ProtobufMessage::parse_from_bytes(&msg.get_content()).unwrap();

        let correlation_id = msg.take_correlation_id();
        let mut msg = Message::new();
        msg.set_message_type(response_type);
        msg.set_correlation_id(correlation_id);
        msg.set_content(response.write_to_bytes().unwrap());
        socket
            .send_multipart(&[&connection_id, &msg.write_to_bytes().unwrap()], 0)
            .unwrap();

        (connection_id, request)
    }

    #[test]
    fn test_zmq_driver() {
        let ctx = zmq::Context::new();
        let socket = ctx.socket(zmq::ROUTER).expect("Failed to create context");
        socket
            .bind("tcp://127.0.0.1:*")
            .expect("Failed to bind socket");
        let addr = socket.get_last_endpoint().unwrap().unwrap();

        // Create the mock engine with this vec so we can refer to it later. Once we put the engine
        // in a box, it is hard to get the vec back out.
        let calls = Arc::new(Mutex::new(Vec::new()));

        // We are going to run two threads to simulate the validator and the driver
        let mock_engine = MockEngine::with(calls.clone());

        let (driver, stop) = ZmqDriver::new();

        let driver_thread = thread::spawn(move || driver.start(&addr, mock_engine));

        let mut response = ConsensusRegisterResponse::new();
        response.set_status(ConsensusRegisterResponse_Status::OK);
        let (connection_id, request): (_, ConsensusRegisterRequest) = recv_rep(
            &socket,
            Message_MessageType::CONSENSUS_REGISTER_REQUEST,
            response,
            Message_MessageType::CONSENSUS_REGISTER_RESPONSE,
        );
        assert!("mock" == request.get_name());
        assert!("0" == request.get_version());
        assert!(
            protocols_from_tuples(vec![("1".into(), "Mock".into())])
                == request.get_additional_protocols()
        );

        let _: ConsensusNotifyAck = send_req_rep(
            &connection_id,
            &socket,
            ConsensusNotifyEngineActivated::new(),
            Message_MessageType::CONSENSUS_NOTIFY_ENGINE_ACTIVATED,
            Message_MessageType::CONSENSUS_NOTIFY_ACK,
        );

        let _: ConsensusNotifyAck = send_req_rep(
            &connection_id,
            &socket,
            ConsensusNotifyPeerConnected::new(),
            Message_MessageType::CONSENSUS_NOTIFY_PEER_CONNECTED,
            Message_MessageType::CONSENSUS_NOTIFY_ACK,
        );

        let _: ConsensusNotifyAck = send_req_rep(
            &connection_id,
            &socket,
            ConsensusNotifyPeerDisconnected::new(),
            Message_MessageType::CONSENSUS_NOTIFY_PEER_DISCONNECTED,
            Message_MessageType::CONSENSUS_NOTIFY_ACK,
        );

        let _: ConsensusNotifyAck = send_req_rep(
            &connection_id,
            &socket,
            ConsensusNotifyPeerMessage::new(),
            Message_MessageType::CONSENSUS_NOTIFY_PEER_MESSAGE,
            Message_MessageType::CONSENSUS_NOTIFY_ACK,
        );

        let _: ConsensusNotifyAck = send_req_rep(
            &connection_id,
            &socket,
            ConsensusNotifyBlockNew::new(),
            Message_MessageType::CONSENSUS_NOTIFY_BLOCK_NEW,
            Message_MessageType::CONSENSUS_NOTIFY_ACK,
        );

        let _: ConsensusNotifyAck = send_req_rep(
            &connection_id,
            &socket,
            ConsensusNotifyBlockValid::new(),
            Message_MessageType::CONSENSUS_NOTIFY_BLOCK_VALID,
            Message_MessageType::CONSENSUS_NOTIFY_ACK,
        );

        let _: ConsensusNotifyAck = send_req_rep(
            &connection_id,
            &socket,
            ConsensusNotifyBlockInvalid::new(),
            Message_MessageType::CONSENSUS_NOTIFY_BLOCK_INVALID,
            Message_MessageType::CONSENSUS_NOTIFY_ACK,
        );

        let _: ConsensusNotifyAck = send_req_rep(
            &connection_id,
            &socket,
            ConsensusNotifyBlockCommit::new(),
            Message_MessageType::CONSENSUS_NOTIFY_BLOCK_COMMIT,
            Message_MessageType::CONSENSUS_NOTIFY_ACK,
        );

        let _: PingResponse = send_req_rep(
            &connection_id,
            &socket,
            PingRequest::new(),
            Message_MessageType::PING_REQUEST,
            Message_MessageType::PING_RESPONSE,
        );

        // Shut it down
        stop.stop();
        driver_thread
            .join()
            .expect("Driver thread panicked")
            .expect("Driver thread returned an error");

        // Assert we did what we expected
        let final_calls = calls.lock().unwrap();
        assert!(contains(&*final_calls, "start"));
        assert!(contains(&*final_calls, "PeerConnected"));
        assert!(contains(&*final_calls, "PeerDisconnected"));
        assert!(contains(&*final_calls, "PeerMessage"));
        assert!(contains(&*final_calls, "BlockNew"));
        assert!(contains(&*final_calls, "BlockValid"));
        assert!(contains(&*final_calls, "BlockInvalid"));
        assert!(contains(&*final_calls, "BlockCommit"));
    }

    fn contains(calls: &Vec<String>, expected: &str) -> bool {
        for call in calls {
            if expected == call.as_str() {
                return true;
            }
        }
        false
    }
}
