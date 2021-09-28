// Copyright 2021 Twitter, Inc.
// Licensed under the Apache License, Version 2.0
// http://www.apache.org/licenses/LICENSE-2.0

#![allow(clippy::unnecessary_unwrap)]

#[macro_use]
extern crate rustcommon_logger;

use boring::ssl::*;
use bytes::BytesMut;
use clap::{App, Arg};
use mio::{Events, Poll, Token};
use mpmc::Queue;
use rand::{Rng, RngCore, SeedableRng};
use rand_distr::Alphanumeric;
use rustcommon_logger::{Level, Logger};
use slab::Slab;
use std::io::Read;
use zstd::Decoder;

use std::borrow::Borrow;
use std::collections::VecDeque;
use std::fs::File;
use std::io::{BufRead, BufReader, ErrorKind};
use std::net::{SocketAddr, ToSocketAddrs};
use std::time::{Duration, Instant};

use rpc_perf::*;

/// TODO(bmartin): this should be consolidated with rpc-perf
use rustcommon_heatmap::AtomicHeatmap;
use rustcommon_heatmap::AtomicU64;
use std::sync::Arc;

// TODO(bmartin): this should be split up into a library and binary
fn main() {
    // initialize logging
    Logger::new()
        .label("rpc-replay")
        .level(Level::Info)
        .init()
        .expect("Failed to initialize logger");

    // process command line arguments
    // TODO(bmartin): consider moving to a file based config
    let matches = App::new("rpc-replay")
        .version("0.0.0")
        .author("Brian Martin <bmartin@twitter.com>")
        .about("Replay cache logs")
        .arg(
            Arg::with_name("trace")
                .long("trace")
                .value_name("FILE")
                .help("zstd compressed cache trace")
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name("binary-trace")
                .long("binary-trace")
                .help("indicates the trace is in the binary format")
                .takes_value(false)
                .required(false),
        )
        .arg(
            Arg::with_name("endpoint")
                .long("endpoint")
                .value_name("HOST:PORT")
                .help("server endpoint to send traffic to")
                .takes_value(true)
                .required(true),
        )
        .arg(
            Arg::with_name("speed")
                .long("speed")
                .value_name("FLOAT")
                .help("replay speed as a multiplier relative to realtime")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("poolsize")
                .long("poolsize")
                .value_name("INT")
                .help("number of connections to open from each worker")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("workers")
                .long("workers")
                .value_name("INT")
                .help("number of client worker threads")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("tls-chain")
                .long("tls-chain")
                .value_name("FILE")
                .help("TLS root cert chain")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("tls-key")
                .long("tls-key")
                .value_name("FILE")
                .help("TLS private key")
                .takes_value(true),
        )
        .arg(
            Arg::with_name("tls-cert")
                .long("tls-cert")
                .value_name("FILE")
                .help("TLS certificate")
                .takes_value(true),
        )
        .get_matches();

    // config value parsing and defaults
    let trace = matches.value_of("trace").unwrap();
    let endpoint = matches.value_of("endpoint").unwrap();
    let speed: f64 = matches
        .value_of("speed")
        .unwrap_or("1.0")
        .parse()
        .expect("invalid value for 'speed'");
    let poolsize: usize = matches
        .value_of("poolsize")
        .unwrap_or("1")
        .parse()
        .expect("invalid value for 'poolsize'");
    let workers: usize = matches
        .value_of("workers")
        .unwrap_or("1")
        .parse()
        .expect("invalid value for 'workers'");
    let binary = matches.is_present("binary-trace");

    // configure tls connector
    let key = matches.value_of("tls-key");
    let cert = matches.value_of("tls-cert");
    let chain = matches.value_of("tls-chain");

    let tls = if key.is_some() && cert.is_some() && chain.is_some() {
        let mut builder = SslConnector::builder(SslMethod::tls_client())
            .expect("failed to initialize TLS client");
        builder.set_verify(SslVerifyMode::NONE);
        builder
            .set_ca_file(chain.unwrap())
            .expect("failed to set TLS CA chain");
        builder
            .set_certificate_file(cert.unwrap(), SslFiletype::PEM)
            .expect("failed to set TLS cert");
        builder
            .set_private_key_file(key.unwrap(), SslFiletype::PEM)
            .expect("failed to set TLS key");
        let connector = builder.build();
        Some(connector)
    } else if key.is_none() && cert.is_none() && chain.is_none() {
        None
    } else {
        fatal!("incomplete TLS config");
    };

    // open files
    let zlog = File::open(trace).expect("failed to open input zlog");
    let zbuf = BufReader::new(zlog);
    let mut log = Decoder::with_buffer(zbuf).expect("failed to init zstd decoder");

    // lookup socket address
    let sockaddr = endpoint.to_socket_addrs().unwrap().next().unwrap();

    // initialize work queue
    let work = Queue::with_capacity(1024 * 1024); // arbitrarily large

    let request_heatmap = Some(Arc::new(AtomicHeatmap::<u64, AtomicU64>::new(
        1_000_000,
        3,
        Duration::from_secs(60),
        Duration::from_millis(1000),
    )));

    // spawn admin
    let mut admin = Admin::for_replay(None);
    admin.set_request_heatmap(request_heatmap.clone());
    let _admin_thread = std::thread::spawn(move || admin.run());

    // spawn workers
    for _ in 0..workers {
        let mut worker = Worker::new(
            sockaddr,
            poolsize,
            tls.clone(),
            work.clone(),
            request_heatmap.clone(),
        );
        std::thread::spawn(move || worker.run());
    }

    // generator state
    let mut ts_sec: u64 = 0;
    let mut sent: usize = 0;
    let mut skip: usize = 0;
    let mut next = Instant::now();

    info!("running...");

    // read trace and generate work
    if binary {
        let mut tmp = [0_u8; 20];
        while log.read_exact(&mut tmp).is_ok() {
            let ts: u64 = u32::from_le_bytes([tmp[0], tmp[1], tmp[2], tmp[3]]) as u64;
            let keyid: u64 = u64::from_le_bytes([
                tmp[4], tmp[5], tmp[6], tmp[7], tmp[8], tmp[9], tmp[10], tmp[11],
            ]);
            let klen_vlen: u32 = u32::from_le_bytes([tmp[12], tmp[13], tmp[14], tmp[15]]);
            let op_ttl: u32 = u32::from_le_bytes([tmp[16], tmp[17], tmp[18], tmp[19]]);
            let op: u8 = (op_ttl >> 24) as u8;
            let ttl: u32 = op_ttl & 0x00FF_FFFF;
            let klen = klen_vlen >> 22;
            let vlen: usize = (klen_vlen & 0x003F_FFFF) as usize;

            // handle new timestamp in log
            if ts > ts_sec {
                let mut now = Instant::now();
                // info!("ts: {} sent: {} skip: {}", ts, sent, skip);
                if ts_sec != 0 {
                    let log_dur = Duration::from_secs(ts - ts_sec).div_f64(speed);
                    next += log_dur;
                    if now > next {
                        debug!("falling behind... try reducing replay rate");
                    }
                }
                ts_sec = ts;
                // delay if needed
                while now < next {
                    std::thread::sleep(Duration::from_micros(100));
                    now = Instant::now();
                }
            }

            let key = format!("{:01$}", keyid, klen as usize);

            let mut request = match op {
                1 => Request::Get { key },
                2 => Request::Gets { key },
                3 => Request::Set { key, vlen, ttl },
                4 => Request::Add { key, vlen, ttl },
                6 => Request::Replace { key, vlen, ttl },
                9 => Request::Delete { key },
                _ => {
                    skip += 1;
                    continue;
                }
            };
            while let Err(r) = work.push(request) {
                request = r;
            }
            sent += 1;
        }
    } else {
        let log = BufReader::new(log);
        let mut lines = log.lines();

        while let Some(Ok(line)) = lines.next() {
            let parts: Vec<&str> = line.split(',').collect();

            let ts: u64 = parts[0].parse::<u64>().expect("invalid timestamp") + 1;
            let verb = parts[5];

            // handle new timestamp in log
            if ts > ts_sec {
                let mut now = Instant::now();
                info!("ts: {} sent: {} skip: {}", ts, sent, skip);
                if ts_sec != 0 {
                    let log_dur = Duration::from_secs(ts - ts_sec).div_f64(speed);
                    next += log_dur;
                    if now > next {
                        warn!("falling behind... try reducing replay rate");
                    }
                }
                ts_sec = ts;
                // delay if needed
                while now < next {
                    std::thread::sleep(Duration::from_micros(100));
                    now = Instant::now();
                }
            }

            let key = parts[1].to_string();
            let vlen: usize = parts[3].parse().expect("failed to parse vlen");
            let ttl: u32 = parts[6].parse().expect("failed to parse ttl");

            let mut request = match verb {
                "get" => Request::Get { key },
                "gets" => Request::Gets { key },
                "set" => Request::Set { key, vlen, ttl },
                "add" => Request::Add { key, vlen, ttl },
                "replace" => Request::Replace { key, vlen, ttl },
                "delete" => Request::Delete { key },
                _ => {
                    skip += 1;
                    continue;
                }
            };
            while let Err(r) = work.push(request) {
                request = r;
            }
            sent += 1;
        }
    }
}

// A very fast PRNG
pub fn rng() -> rand_xoshiro::Xoshiro256PlusPlus {
    rand_xoshiro::Xoshiro256PlusPlus::seed_from_u64(0)
}

struct Worker {
    sessions: Slab<Session>,
    ready_queue: VecDeque<Token>,
    poll: Poll,
    work: Queue<Request>,
    request_heatmap: Option<Arc<AtomicHeatmap<u64, AtomicU64>>>,
    rng: rand_xoshiro::Xoshiro256PlusPlus,
}

impl Worker {
    pub fn new(
        addr: SocketAddr,
        poolsize: usize,
        tls: Option<SslConnector>,
        work: Queue<Request>,
        request_heatmap: Option<Arc<AtomicHeatmap<u64, AtomicU64>>>,
    ) -> Self {
        let poll = mio::Poll::new().unwrap();

        let mut sessions: Slab<Session> = Slab::with_capacity(poolsize);

        let mut ready_queue: VecDeque<Token> = VecDeque::with_capacity(poolsize);

        for _ in 0..poolsize {
            let mut session = Session::new(addr);
            session
                .connect(tls.as_ref(), false)
                .expect("failed to connect");
            let entry = sessions.vacant_entry();
            let token = Token(entry.key());
            ready_queue.push_back(token);
            session.set_token(token);
            session.register(&poll).expect("register failed");
            entry.insert(session);
        }

        Self {
            sessions,
            ready_queue,
            poll,
            work,
            request_heatmap,
            rng: rng(),
        }
    }

    pub fn send_request(&mut self, token: Token, request: Request) {
        let session = self.sessions.get_mut(token.0).expect("bad token");
        REQUEST.increment();
        match request {
            Request::Get { key } => {
                REQUEST_GET.increment();
                session
                    .write_buffer
                    .extend_from_slice(format!("get {}\r\n", key).as_bytes());
                debug!("get {}", key);
            }
            Request::Gets { key } => {
                REQUEST_GET.increment();
                session
                    .write_buffer
                    .extend_from_slice(format!("gets {}\r\n", key).as_bytes());
                debug!("get {}", key);
            }
            Request::Set { key, vlen, ttl } => {
                let value = (&mut self.rng as &mut dyn RngCore)
                    .sample_iter(&Alphanumeric)
                    .take(vlen)
                    .collect::<Vec<u8>>();
                session
                    .write_buffer
                    .extend_from_slice(format!("set {} 0 {} {}\r\n", key, ttl, vlen).as_bytes());
                session.write_buffer.extend_from_slice(&value);
                session.write_buffer.extend_from_slice(b"\r\n");
                debug!("set {} 0 {} {}", key, ttl, vlen);
            }
            Request::Add { key, vlen, ttl } => {
                let value = (&mut self.rng as &mut dyn RngCore)
                    .sample_iter(&Alphanumeric)
                    .take(vlen)
                    .collect::<Vec<u8>>();
                session
                    .write_buffer
                    .extend_from_slice(format!("add {} 0 {} {}\r\n", key, ttl, vlen).as_bytes());
                session.write_buffer.extend_from_slice(&value);
                session.write_buffer.extend_from_slice(b"\r\n");
                debug!("add {} 0 {} {}", key, ttl, vlen);
            }
            Request::Replace { key, vlen, ttl } => {
                let value = (&mut self.rng as &mut dyn RngCore)
                    .sample_iter(&Alphanumeric)
                    .take(vlen)
                    .collect::<Vec<u8>>();
                session.write_buffer.extend_from_slice(
                    format!("replace {} 0 {} {}\r\n", key, ttl, vlen).as_bytes(),
                );
                session.write_buffer.extend_from_slice(&value);
                session.write_buffer.extend_from_slice(b"\r\n");
                debug!("replace {} 0 {} {}", key, ttl, vlen);
            }
            Request::Delete { key } => {
                session
                    .write_buffer
                    .extend_from_slice(format!("delete {}\r\n", key).as_bytes());
                debug!("delete {}", key);
            }
        }
        let _ = session.flush();
        if session.write_pending() > 0 {
            let _ = session.reregister(&self.poll);
        }
    }

    pub fn run(&mut self) {
        let mut events = Events::with_capacity(1024);
        loop {
            if let Some(token) = self.ready_queue.pop_front() {
                if let Some(request) = self.work.pop() {
                    self.send_request(token, request);
                } else {
                    self.ready_queue.push_front(token);
                }
            }

            let _ = self
                .poll
                .poll(&mut events, Some(std::time::Duration::from_millis(1)));

            for event in &events {
                let token = event.token();
                let session = self.sessions.get_mut(token.0).expect("unknown token");

                // handle error events first
                if event.is_error() {
                    let _ = session.deregister(&self.poll);
                    session.close();
                    warn!("error");
                }

                // handle handshaking
                if session.is_handshaking() {
                    if let Err(e) = session.do_handshake() {
                        if e.kind() != ErrorKind::WouldBlock {
                            let _ = session.deregister(&self.poll);
                            session.close();
                            warn!("error");
                        }
                    }
                    if session.is_handshaking() {
                        let _ = session.reregister(&self.poll);
                        continue;
                    }
                }

                // handle reads
                if event.is_readable() {
                    match session.read() {
                        Ok(None) => {
                            continue;
                        }
                        Ok(Some(0)) => {
                            let _ = session.deregister(&self.poll);
                            session.close();
                            warn!("server hangup");
                        }
                        Ok(Some(_)) => match decode(&mut session.read_buffer) {
                            Ok(_) => {
                                RESPONSE.increment();
                                if let Some(ref heatmap) = self.request_heatmap {
                                    let now = Instant::now();
                                    let elapsed = now - session.timestamp();
                                    let us = (elapsed.as_secs_f64() * 1_000_000.0) as u64;
                                    heatmap.increment(now, us, 1);
                                }
                                if let Some(request) = self.work.pop() {
                                    self.send_request(token, request);
                                } else {
                                    self.ready_queue.push_back(token);
                                }

                                continue;
                            }
                            Err(ParseError::Incomplete) => {
                                continue;
                            }
                            Err(_) => {
                                let _ = session.deregister(&self.poll);
                                session.close();
                                warn!("parse error");
                            }
                        },
                        Err(_) => {
                            let _ = session.deregister(&self.poll);
                            session.close();
                            warn!("read error");
                        }
                    }
                }

                // handle writes
                if event.is_writable() && !session.write_buffer.is_empty() {
                    session.flush().expect("flush failed");
                    if session.write_pending() > 0 {
                        let _ = session.reregister(&self.poll);
                    }
                }
            }
        }
    }
}

pub enum Request {
    Get { key: String },
    Gets { key: String },
    Set { key: String, vlen: usize, ttl: u32 },
    Add { key: String, vlen: usize, ttl: u32 },
    Replace { key: String, vlen: usize, ttl: u32 },
    Delete { key: String },
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParseError {
    Incomplete,
    Error,
    Unknown,
}

// this is a very barebones memcache parser
fn decode(buffer: &mut BytesMut) -> Result<(), ParseError> {
    // no-copy borrow as a slice
    let buf: &[u8] = (*buffer).borrow();

    for response in &[
        "STORED\r\n",
        "NOT_STORED\r\n",
        "EXISTS\r\n",
        "NOT_FOUND\r\n",
        "DELETED\r\n",
        "TOUCHED\r\n",
    ] {
        let bytes = response.as_bytes();
        if buf.len() >= bytes.len() && &buf[0..bytes.len()] == bytes {
            let _ = buffer.split_to(bytes.len());
            return Ok(());
        }
    }

    let mut windows = buf.windows(5);
    if let Some(response_end) = windows.position(|w| w == b"END\r\n") {
        if response_end > 0 {
            RESPONSE_HIT.increment();
        }
        let _ = buffer.split_to(response_end + 5);
        return Ok(());
    }

    Err(ParseError::Incomplete)
}