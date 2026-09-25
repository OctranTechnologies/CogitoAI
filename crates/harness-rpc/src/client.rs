use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

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

#[derive(Clone)]
pub struct RpcClientReader {
    reader: Arc<Mutex<BufReader<TcpStream>>>,
    pending: Arc<Mutex<Vec<ServerMessage>>>,
}

#[derive(Clone)]
pub struct RpcClientWriter {
    writer: Arc<Mutex<BufWriter<TcpStream>>>,
    next_id: Arc<AtomicU64>,
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
        let id = write_request(&mut self.writer, &mut self.next_id, method, params)?;
        loop {
            match read_message(&mut self.reader)? {
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
        read_message(&mut self.reader)
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

    pub fn split(self) -> (RpcClientReader, RpcClientWriter) {
        (
            RpcClientReader {
                reader: Arc::new(Mutex::new(self.reader)),
                pending: Arc::new(Mutex::new(self.pending)),
            },
            RpcClientWriter {
                writer: Arc::new(Mutex::new(self.writer)),
                next_id: Arc::new(AtomicU64::new(self.next_id)),
            },
        )
    }
}

impl RpcClientReader {
    pub fn receive(&self) -> Result<ServerMessage, RpcClientError> {
        if let Some(message) = self
            .pending
            .lock()
            .expect("RPC pending lock poisoned")
            .pop()
        {
            return Ok(message);
        }
        let mut reader = self.reader.lock().expect("RPC reader lock poisoned");
        read_message(&mut *reader)
    }
}

impl RpcClientWriter {
    pub fn request(&self, method: &str, params: Value) -> Result<String, RpcClientError> {
        let mut next_id = self.next_id.load(Ordering::Relaxed);
        let mut writer = self.writer.lock().expect("RPC writer lock poisoned");
        let id = write_request(&mut *writer, &mut next_id, method, params)?;
        self.next_id.store(next_id, Ordering::Relaxed);
        Ok(id)
    }

    pub fn shutdown(&self) {
        if let Ok(writer) = self.writer.lock() {
            let _ = writer.get_ref().shutdown(Shutdown::Both);
        }
    }
}

fn write_request<W: Write>(
    writer: &mut W,
    next_id: &mut u64,
    method: &str,
    params: Value,
) -> Result<String, RpcClientError> {
    *next_id += 1;
    let id = format!("request-{}", next_id);
    let request = RpcRequest {
        version: RPC_PROTOCOL_VERSION,
        id: Some(id.clone()),
        method: method.to_owned(),
        params,
    };
    let bytes = serde_json::to_vec(&request)?;
    if bytes.len() > MAX_LINE_BYTES {
        return Err(RpcClientError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "RPC request exceeds the maximum line size",
        )));
    }
    writer.write_all(&bytes)?;
    writer.write_all(b"\n")?;
    writer.flush()?;
    Ok(id)
}

fn read_message<R: BufRead>(reader: &mut R) -> Result<ServerMessage, RpcClientError> {
    let mut line = String::new();
    let read = reader.read_line(&mut line)?;
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
