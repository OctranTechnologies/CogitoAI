use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::net::{SocketAddr, TcpStream};

use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::protocol::{RpcRequest, RpcResponse, ServerMessage, RPC_PROTOCOL_VERSION};

const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum RpcClientError {
    #[error("RPC I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("RPC JSON failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("RPC connection closed before a response arrived")]
    ConnectionClosed,
}

pub struct RpcClient {
    reader: BufReader<TcpStream>,
    writer: BufWriter<TcpStream>,
    next_id: u64,
    pending: Vec<ServerMessage>,
}

impl RpcClient {
    pub fn connect(address: SocketAddr) -> Result<Self, RpcClientError> {
        let stream = TcpStream::connect(address)?;
        let writer = BufWriter::new(stream.try_clone()?);
        Ok(Self {
            reader: BufReader::new(stream),
            writer,
            next_id: 0,
            pending: Vec::new(),
        })
    }

    pub fn request(&mut self, method: &str, params: Value) -> Result<RpcResponse, RpcClientError> {
        self.next_id += 1;
        let id = format!("request-{}", self.next_id);
        let request = RpcRequest {
            version: RPC_PROTOCOL_VERSION,
            id: Some(id.clone()),
            method: method.to_owned(),
            params,
        };
        self.write_value(&request)?;
        loop {
            match self.read_message()? {
                ServerMessage::Response(response)
                    if response.id.as_deref() == Some(id.as_str()) =>
                {
                    return Ok(response);
                }
                message => self.pending.push(message),
            }
        }
    }

    pub fn receive(&mut self) -> Result<ServerMessage, RpcClientError> {
        if let Some(message) = self.pending.pop() {
            return Ok(message);
        }
        self.read_message()
    }

    pub fn receive_response<T: DeserializeOwned>(&mut self) -> Result<T, RpcClientError> {
        match self.receive()? {
            ServerMessage::Response(response) => {
                serde_json::from_value(response.result.ok_or_else(|| {
                    serde_json::Error::io(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "RPC response did not contain a result",
                    ))
                })?)
                .map_err(RpcClientError::from)
            }
            ServerMessage::Notification(_) => Err(RpcClientError::ConnectionClosed),
        }
    }

    fn write_value<T: serde::Serialize>(&mut self, value: &T) -> Result<(), RpcClientError> {
        let bytes = serde_json::to_vec(value)?;
        if bytes.len() > MAX_LINE_BYTES {
            return Err(RpcClientError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "RPC request exceeds the maximum line size",
            )));
        }
        self.writer.write_all(&bytes)?;
        self.writer.write_all(b"\n")?;
        self.writer.flush()?;
        Ok(())
    }

    fn read_message(&mut self) -> Result<ServerMessage, RpcClientError> {
        let mut line = String::new();
        let read = self.reader.read_line(&mut line)?;
        if read == 0 {
            return Err(RpcClientError::ConnectionClosed);
        }
        if line.len() > MAX_LINE_BYTES {
            return Err(RpcClientError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "RPC response exceeds the maximum line size",
            )));
        }
        Ok(serde_json::from_str(&line)?)
    }
}
