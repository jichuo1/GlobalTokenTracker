//! A persistent private state owner and decision worker. It writes only the
//! coordinator database, never calls models or writes workspaces. EOF ends its
//! lifetime; requests are never replayed internally.
use globaltokentracker_core::scheduling_kernel;
use serde_json::{Value, json};
use std::io::{self, BufRead, BufWriter, Write};

const FRAME_LIMIT: usize = 16 * 1024 * 1024;
const MESSAGE_LIMIT: usize = 256 * 1024 * 1024;

fn invalid() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, "invalid scheduler frame")
}

fn read_message(input: &mut impl BufRead) -> io::Result<Option<Value>> {
    let mut assembled = String::new();
    let mut sequence = 0_u64;
    let mut total = None;
    loop {
        let mut line = Vec::new();
        loop {
            let chunk = input.fill_buf()?;
            if chunk.is_empty() {
                return Ok(None);
            }
            let count = chunk
                .iter()
                .position(|b| *b == b'\n')
                .map_or(chunk.len(), |n| n + 1);
            if line.len() + count > FRAME_LIMIT {
                return Err(invalid());
            }
            let done = chunk[count - 1] == b'\n';
            line.extend_from_slice(&chunk[..count]);
            input.consume(count);
            if done {
                break;
            }
        }
        let value: Value = serde_json::from_slice(&line).map_err(|_| invalid())?;
        if value.get("wire_chunk").is_none() {
            return if sequence == 0 {
                Ok(Some(value))
            } else {
                Err(invalid())
            };
        }
        let size = value["size"].as_u64().ok_or_else(invalid)? as usize;
        if size > MESSAGE_LIMIT
            || value["wire_chunk"].as_u64() != Some(sequence)
            || total.is_some_and(|v| v != size)
        {
            return Err(invalid());
        }
        total = Some(size);
        assembled.push_str(value["data"].as_str().ok_or_else(invalid)?);
        if assembled.len() > size {
            return Err(invalid());
        }
        sequence += 1;
        if value["last"].as_bool() == Some(true) {
            if assembled.len() != size {
                return Err(invalid());
            }
            return serde_json::from_str(&assembled)
                .map(Some)
                .map_err(|_| invalid());
        }
    }
}

fn write_message(output: &mut impl Write, value: &Value) -> io::Result<()> {
    let encoded = serde_json::to_string(value)?;
    if encoded.len() > MESSAGE_LIMIT {
        return Err(invalid());
    }
    if encoded.len() < FRAME_LIMIT {
        output.write_all(encoded.as_bytes())?;
        output.write_all(b"\n")?;
    } else {
        let mut start = 0;
        let mut sequence = 0;
        while start < encoded.len() {
            let mut end = (start + 1024 * 1024).min(encoded.len());
            while !encoded.is_char_boundary(end) {
                end -= 1;
            }
            serde_json::to_writer(
                &mut *output,
                &json!({"wire_chunk":sequence,"size":encoded.len(),"last":end==encoded.len(),"data":&encoded[start..end]}),
            )?;
            output.write_all(b"\n")?;
            start = end;
            sequence += 1;
        }
    }
    output.flush()
}

fn main() -> io::Result<()> {
    let mut kernel = scheduling_kernel::Kernel::default();
    let mut input = io::stdin().lock();
    // serde_json writes many tiny fragments. Buffer one response so those do
    // not become hundreds of pipe writes/context switches on Windows.
    let stdout = io::stdout();
    let mut output = BufWriter::with_capacity(64 * 1024, stdout.lock());
    loop {
        let Some(request) = read_message(&mut input)? else {
            return Ok(());
        };
        let result = kernel.handle(&request);
        let response = match result {
            Ok(result) => json!({"result":result}),
            Err(error) => json!({"error":error}),
        };
        write_message(&mut output, &response)?;
    }
}
