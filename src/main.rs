extern crate futures;
extern crate futures_cpupool;
extern crate tokio_core;
extern crate tokio_proto;
extern crate tokio_service;

use futures::{future, Future, BoxFuture, Stream, Sink};
use futures_cpupool::CpuPool;
use std::io;
use std::net::SocketAddr;
use std::str;
use tokio_core::io::{Codec, EasyBuf, Io, Framed};
use tokio_proto::TcpServer;
use tokio_proto::pipeline::ServerProto;
use tokio_service::{Service, NewService};

#[derive(Debug, PartialEq)]
pub enum SmtpCommand {
    Ehlo { identity: String },
    MailFrom { from: String },
    RcptTo { to: String },
    Data,
    Quit,
    DataContinuation,
    DataComplete { data: String }
}

#[derive(Debug)]
pub struct SmtpResponse {
    code: i16,
    text: String
}

#[derive(PartialEq)]
pub enum SmtpCodecState {
    WaitingForEhlo,
    WaitingForMailFrom,
    WaitingForRcptTo,
    WaitingForData,
    ReceivingData,
    WaitingForClose
}

pub struct SmtpCodec {
    state: SmtpCodecState,
    client_identifier: String,
    mail_from: String,
    rcpt_to: String,
    data_buffer: String
}

impl Codec for SmtpCodec {
    type In = SmtpCommand;
    type Out = Option<SmtpResponse>;

    fn decode(&mut self, buf: &mut EasyBuf) -> io::Result<Option<Self::In>> {
        use SmtpCodecState::*;
        if self.state == WaitingForClose {
            return Err(io::Error::new(io::ErrorKind::Other, "received data after QUIT"));
        }
        if let Some(i) = buf.as_slice().iter().position(|&b| b == b'\r') {
            let line = buf.drain_to(i);
            buf.drain_to(2);
            let line = str::from_utf8(line.as_slice()).unwrap();
            if self.state == ReceivingData {
                return if line != "." {
                    self.data_buffer.push_str(line);
                    self.data_buffer.push('\n');
                    Ok(Some(SmtpCommand::DataContinuation))
                }
                else {
                    self.state = WaitingForMailFrom;
                    let res = SmtpCommand::DataComplete { data: self.data_buffer.clone() };
                    self.data_buffer = "".to_string();
                    Ok(Some(res))
                }
            }
            if line.starts_with("EHLO ") && self.state == WaitingForEhlo {
                let parts: Vec<&str> = line.split(" ").skip(1).collect();
                self.client_identifier = parts.join(" ");
                self.state = WaitingForMailFrom;
                Ok(Some(SmtpCommand::Ehlo { identity: self.client_identifier.clone() }))
            }
            else if line.starts_with("MAIL FROM:") && self.state == WaitingForMailFrom {
                let parts: Vec<&str> = line.split(":").skip(1).collect();
                self.mail_from = parts.join(":");
                self.state = WaitingForRcptTo;
                Ok(Some(SmtpCommand::MailFrom { from: self.mail_from.clone() }))
            }
            else if line.starts_with("RCPT TO:") && self.state == WaitingForRcptTo {
                let parts: Vec<&str> = line.split(":").skip(1).collect();
                self.rcpt_to = parts.join(":");
                self.state = WaitingForData;
                Ok(Some(SmtpCommand::RcptTo { to: self.rcpt_to.clone() }))
            }
            else if line == "DATA" && self.state == WaitingForData {
                self.state = ReceivingData;
                Ok(Some(SmtpCommand::Data))
            }
            else if line == "QUIT" {
                self.state = WaitingForClose;
                Ok(Some(SmtpCommand::Quit))
            }
            else {
                Err(io::Error::new(io::ErrorKind::Other, "invalid SMTP command"))
            }
        }
        else {
            Ok(None)
        }
    }

    fn encode(&mut self, msg: Self::Out, buf: &mut Vec<u8>) -> io::Result<()> {
        if let Some(response) = msg {
            buf.extend(format!("{} {}\r\n", response.code, response.text).as_bytes());
        }
        Ok(())
    }
}

pub struct SmtpProto;

impl<T: Io + 'static> ServerProto<T> for SmtpProto {
    type Request = SmtpCommand;
    type Response = Option<SmtpResponse>;

    type Transport = Framed<T, SmtpCodec>;
    type BindTransport = Box<Future<Item = Self::Transport, Error = io::Error>>;
    fn bind_transport(&self, io: T) -> Self::BindTransport {
        let codec = SmtpCodec { state: SmtpCodecState::WaitingForEhlo, client_identifier: "".to_string(), mail_from: "".to_string(), rcpt_to: "".to_string(), data_buffer: "".to_string() };
        let transport = io.framed(codec);
        let handshake = transport.send(Some(SmtpResponse { code: 220, text: "rust-mailer ESMTP rust-mailer".to_string() }))
            .and_then(|t| t.into_future().map_err(|(e, _)| e))
            .and_then(|(command, transport)| {
                match command {
                    Some(SmtpCommand::Ehlo { identity: other_server }) => {
                        println!("Received EHLO");
                        let response = format!("Hi {}, I am glad to meet you", other_server);
                        Box::new(transport.send(Some(SmtpResponse { code: 250, text: response }))) as Self::BindTransport
                    }
                    _ => {
                        println!("Invalid handshake: {:?}", command);
                        let err = io::Error::new(io::ErrorKind::Other, "invalid handshake");
                        Box::new(future::err(err)) as Self::BindTransport
                    }
                }
            });
        Box::new(handshake)
    }
}

pub struct Smtp {
    thread_pool: CpuPool
}

impl Service for Smtp {
    type Request = SmtpCommand;
    type Response = Option<SmtpResponse>;
    type Error = io::Error;
    type Future = BoxFuture<Self::Response, Self::Error>;

    fn call(&self, req: Self::Request) -> Self::Future {
        self.thread_pool.spawn_fn(move || {
            use SmtpCommand::*;
            match req {
                DataContinuation => Ok(None),
                DataComplete { data } => {
                    let len = data.len();
                    Ok(Some(SmtpResponse { code: 200, text: format!("Delivered {} bytes", len) } ))
                },
                _ => Ok(Some(SmtpResponse { code: 200, text: format!("{:?}", req) }))
            }
        }).boxed()
    }
}

fn serve<T>(addr: SocketAddr, new_service: T) where T: NewService<Request = SmtpCommand, Response = Option<SmtpResponse>, Error = io::Error> + Send + Sync + 'static, {
    TcpServer::new(SmtpProto, addr).serve(new_service)
}

fn main() {
    let addr = "127.0.0.1:2525".parse().unwrap();
    let threads = CpuPool::new_num_cpus();
    serve(addr, move || {
        Ok(Smtp { thread_pool: threads.clone() })
    });
}
