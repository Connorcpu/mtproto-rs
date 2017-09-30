extern crate byteorder;
extern crate crc;
extern crate env_logger;
#[macro_use]
extern crate error_chain;
extern crate futures;
extern crate log;
extern crate mtproto;
extern crate rand;
extern crate serde;
extern crate serde_mtproto;
extern crate tokio_core;
extern crate tokio_io;
extern crate toml;


use std::fs;
use std::io::{self, Read};

use byteorder::{BigEndian, ByteOrder, LittleEndian};
use crc::crc32;
use futures::Future;
use mtproto::rpc::{AppId, Session};
use mtproto::rpc::message::{Message, MessageType};
use mtproto::rpc::encryption::asymm;
use mtproto::schema;
use rand::{Rng, ThreadRng};
use serde::Serialize;
use serde_mtproto::{Identifiable, MtProtoSized};
use tokio_core::net::TcpStream;
use tokio_core::reactor::{Core, Handle};


mod error {
    error_chain! {
        links {
            MtProto(::mtproto::Error, ::mtproto::ErrorKind);
            SerdeMtProto(::serde_mtproto::Error, ::serde_mtproto::ErrorKind);
        }

        foreign_links {
            Io(::std::io::Error);
            SetLogger(::log::SetLoggerError);
            TomlDeserialize(::toml::de::Error);
        }
    }
}

use error::ResultExt;

macro_rules! tryf {
    ($e:expr) => {
        match { $e } {
            Ok(v) => v,
            Err(e) => return Box::new(futures::future::err(e.into())),
        }
    }
}


fn auth(handle: Handle) -> Box<Future<Item = (), Error = error::Error>> {
    let app_id = tryf!(load_app_id());

    let remote_addr = "149.154.167.51:443".parse().unwrap();
    println!("Address: {:?}", &remote_addr);
    let socket = TcpStream::connect(&remote_addr, &handle).map_err(error::Error::from);

    let process = socket.and_then(|socket|
        -> Box<Future<Item = (TcpStream, Vec<u8>, Session, ThreadRng, u32), Error = error::Error>>
    {
        let mut rng = rand::thread_rng();
        let mut session = Session::new(rng.gen(), app_id);
        let send_counter = 0;

        let req_pq = schema::rpc::req_pq {
            nonce: rng.gen(),
        };

        let serialized_message = tryf!(create_serialized_message(&mut session, req_pq, MessageType::PlainText));

        //let request = create_tcp_request_abridged(socket, serialized_message, true);
        //let request = create_tcp_request_intermediate(socket, serialized_message, true);
        let (request, send_counter) = create_tcp_request_full(socket, serialized_message, send_counter);

        Box::new(request.map(move |(s, b)| (s, b, session, rng, send_counter)))
    }).and_then(|(socket, response_bytes, mut session, mut rng, send_counter)|
        -> Box<Future<Item = (TcpStream, Vec<u8>, Session, ThreadRng, u32), Error = error::Error>>
    {
        println!("Response bytes: {:?}", &response_bytes);
        let response: Message<schema::ResPQ> = tryf!(session.process_message(&response_bytes));
        println!("Message received: {:#?}", &response);

        let res_pq = match response {
            Message::PlainText { body, .. } => body.into_inner().into_inner(),
            _ => unreachable!(),
        };

        // FIXME: check nonces' equality here

        let pq_u64 = BigEndian::read_u64(&res_pq.pq);
        println!("Decomposing pq = {}...", pq_u64);
        let (p_u32, q_u32) = tryf!(asymm::decompose_pq(pq_u64));
        println!("Decomposed p = {}, q = {}", p_u32, q_u32);
        let u32_to_vec = |num| {
            let mut v = vec![0; 4];
            BigEndian::write_u32(v.as_mut_slice(), num);
            v
        };
        let p = u32_to_vec(p_u32);
        let q = u32_to_vec(q_u32);

        let p_q_inner_data = schema::P_Q_inner_data {
            pq:  res_pq.pq,
            p: p.clone().into(),
            q: q.clone().into(),
            nonce: res_pq.nonce,
            server_nonce: res_pq.server_nonce,
            new_nonce: rng.gen(),
        };

        println!("Data to send: {:#?}", &p_q_inner_data);
        let p_q_inner_data_serialized = tryf!(serde_mtproto::to_bytes(&p_q_inner_data));
        println!("Data bytes to send: {:?}", &p_q_inner_data_serialized);
        let known_sha1_fingerprints = tryf!(asymm::KNOWN_RAW_KEYS.iter()
            .map(|raw_key| {
                let sha1_fingerprint = raw_key.read()?.sha1_fingerprint()?;
                Ok(sha1_fingerprint.iter().map(|b| format!("{:02x}", b)).collect::<String>())
            })
            .collect::<error::Result<Vec<_>>>());
        println!("Known public key SHA1 fingerprints: {:?}", known_sha1_fingerprints);
        let known_fingerprints = tryf!(asymm::KNOWN_RAW_KEYS.iter()
            .map(|raw_key| Ok(raw_key.read()?.fingerprint()?))
            .collect::<error::Result<Vec<_>>>());
        println!("Known public key fingerprints: {:?}", known_fingerprints);
        let server_pk_fingerprints = res_pq.server_public_key_fingerprints.inner().as_slice();
        println!("Server public key fingerprints: {:?}", &server_pk_fingerprints);
        let (rsa_public_key, fingerprint) =
            tryf!(asymm::find_first_key_fail_safe(server_pk_fingerprints));
        println!("RSA public key used: {:#?}", &rsa_public_key);
        let encrypted_data = tryf!(rsa_public_key.encrypt(&p_q_inner_data_serialized));
        println!("Encrypted data: {:?}", encrypted_data.as_ref());

        let req_dh_params = schema::rpc::req_DH_params {
            nonce: res_pq.nonce,
            server_nonce: res_pq.server_nonce,
            p: p.into(),
            q: q.into(),
            public_key_fingerprint: fingerprint,
            encrypted_data: encrypted_data.to_vec().into(),
        };

        let serialized_message = tryf!(create_serialized_message(&mut session, req_dh_params, MessageType::PlainText));

        //let request = create_tcp_request_abridged(socket, serialized_message, false);
        //let request = create_tcp_request_intermediate(socket, serialized_message, false);
        let (request, send_counter) = create_tcp_request_full(socket, serialized_message, send_counter);

        Box::new(request.map(move |(s, b)| (s, b, session, rng, send_counter)))
    }).and_then(|(_socket, response_bytes, session, _rng, _send_counter)| {
        println!("Response bytes: {:?}", &response_bytes);
        let response: Message<schema::Server_DH_Params> = tryf!(session.process_message(&response_bytes));
        println!("Message received: {:#?}", &response);

        Box::new(futures::future::ok(()))
    });

    Box::new(process)
}

fn load_app_id() -> error::Result<AppId> {
    let mut config_data = String::new();
    let mut file = fs::File::open("app_id.toml")
        .chain_err(|| "this example needs a app_id.toml file with `api_id` and `api_hash` fields in it")?;

    file.read_to_string(&mut config_data)?;
    let app_id = toml::from_str(&config_data)?;

    Ok(app_id)
}

fn create_serialized_message<T>(session: &mut Session,
                                data: T,
                                message_type: MessageType)
                               -> error::Result<Vec<u8>>
    where T: ::std::fmt::Debug + Serialize + Identifiable + MtProtoSized
{
    let message = session.create_message(data, message_type)?;
    println!("Message to send: {:#?}", &message);
    let serialized_message = serde_mtproto::to_bytes(&message)?;
    println!("Request bytes: {:?}", &serialized_message);

    Ok(serialized_message)
}

fn create_tcp_request_full(socket: TcpStream,
                           serialized_message: Vec<u8>,
                           mut send_counter: u32)
                          -> (Box<Future<Item = (TcpStream, Vec<u8>), Error = error::Error>>, u32) {
    let len = serialized_message.len() + 12;
    let data = if len <= 0xff_ff_ff_ff {
        let mut data = vec![0; len];

        LittleEndian::write_u32(&mut data[0..4], len as u32);
        LittleEndian::write_u32(&mut data[4..8], send_counter);
        data[8..len-4].copy_from_slice(&serialized_message);

        let crc = crc32::checksum_ieee(&data[0..len-4]);
        send_counter += 1;

        LittleEndian::write_u32(&mut data[len-4..], crc);

        data
    } else {
        panic!("Message of length {} too long to send"); // FIXME
    };

    let request = tokio_io::io::write_all(socket, data);

    let response = request.and_then(|(socket, _request_bytes)| {
        tokio_io::io::read_exact(socket, [0; 8])
    }).and_then(|(socket, first_bytes)| {
        let len = LittleEndian::read_u32(&first_bytes[0..4]);
        let ulen = len as usize;
        // TODO: Check seq_no
        let _seq_no = LittleEndian::read_u32(&first_bytes[4..8]);

        tokio_io::io::read_exact(socket, vec![0; ulen - 8]).and_then(move |(socket, last_bytes)| {
            let checksum = LittleEndian::read_u32(&last_bytes[ulen - 12..ulen - 8]);
            let mut body = last_bytes;
            body.truncate(ulen - 12);

            let mut value = 0;
            value = crc32::update(value, &crc32::IEEE_TABLE, &first_bytes[0..4]);
            value = crc32::update(value, &crc32::IEEE_TABLE, &first_bytes[4..8]);
            value = crc32::update(value, &crc32::IEEE_TABLE, &body);

            if value != checksum {
                futures::future::err(io::Error::new(io::ErrorKind::Other, "wrong checksum"))
            } else {
                futures::future::ok((socket, body))
            }
        })
    });

    (Box::new(response.map_err(Into::into)), send_counter)
}

fn create_tcp_request_intermediate(socket: TcpStream,
                                   serialized_message: Vec<u8>,
                                   is_first_request: bool)
                                  -> Box<Future<Item = (TcpStream, Vec<u8>), Error = error::Error>> {
    let len = serialized_message.len();
    let data = if len <= 0xff_ff_ff_ff {
        let mut data = vec![0; 4 + len];

        LittleEndian::write_u32(&mut data[0..4], len as u32);
        data[4..].copy_from_slice(&serialized_message);

        data
    } else {
        panic!("Message of length {} too long to send"); // FIXME
    };

    let init: Box<Future<Item = (TcpStream, &'static [u8]), Error = io::Error>> = if is_first_request {
        Box::new(tokio_io::io::write_all(socket, b"\xee\xee\xee\xee".as_ref()))
    } else {
        Box::new(futures::future::ok((socket, [].as_ref())))
    };

    let request = init.and_then(|(socket, _init_bytes)| {
        tokio_io::io::write_all(socket, data)
    });

    let response = request.and_then(|(socket, _request_bytes)| {
        tokio_io::io::read_exact(socket, [0; 4])
    }).and_then(|(socket, bytes_len)| {
        let len = LittleEndian::read_u32(&bytes_len);
        tokio_io::io::read_exact(socket, vec![0; len as usize]) // Use safe cast
    });

    Box::new(response.map_err(Into::into))
}

fn create_tcp_request_abridged(socket: TcpStream,
                               serialized_message: Vec<u8>,
                               is_first_request: bool)
                              -> Box<Future<Item = (TcpStream, Vec<u8>), Error = error::Error>> {
    let mut data = if is_first_request { vec![0xef] } else { vec![] };
    let len = serialized_message.len() / 4;

    if len < 0x7f {
        data.push(len as u8);
    } else if len < 0xff_ff_ff {
        data.push(0x7f);
        LittleEndian::write_uint(&mut data, len as u64, 3); // Use safe cast here
    } else {
        panic!("Message of length {} too long to send");
    }

    data.extend(serialized_message);
    let request = tokio_io::io::write_all(socket, data);

    let response = request.and_then(|(socket, _request_bytes)| {
        tokio_io::io::read_exact(socket, [0; 1])
    }).and_then(|(socket, byte_id)| {
        let boxed: Box<Future<Item = (TcpStream, usize), Error = io::Error>> = if byte_id == [0x7f] {
            Box::new(tokio_io::io::read_exact(socket, [0; 3]).map(|(socket, bytes_len)| {
                let len = LittleEndian::read_uint(&bytes_len, 3) as usize * 4;
                (socket, len)
            }))
        } else {
            Box::new(futures::future::ok((socket, byte_id[0] as usize * 4)))
        };

        boxed
    }).and_then(|(socket, len)| {
        tokio_io::io::read_exact(socket, vec![0; len])
    });

    Box::new(response.map_err(Into::into))
}


fn run() -> error::Result<()> {
    env_logger::init()?;
    let mut core = Core::new()?;

    let auth_future = auth(core.handle());
    core.run(auth_future)?;

    Ok(())
}

quick_main!(run);
