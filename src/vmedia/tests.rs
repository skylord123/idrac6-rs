use super::*;
use std::net::TcpListener;

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

#[test]
fn login_packet_layout() {
    let p = proto::login("vmuser", "secret", true);
    assert_eq!(p.len(), 153);
    assert_eq!(&p[0..4], b"AVMP");
    assert_eq!(be32(&p, 4), 153);
    assert_eq!(&p[8..10], &[0x01, 0x00]);
    assert_eq!(p[12], 6);
    assert_eq!(&p[13..19], b"vmuser");
    assert_eq!(&p[109..115], b"secret");
    assert_eq!(p[152], 1);
}

#[test]
fn apcp_roundtrip() {
    let rnd = [7u8; 32];
    let req = proto::apcp_request(2, 1, &rnd);
    assert_eq!(req.len(), 53);
    assert_eq!(req[12], 2);
    // Reply captured from a real iDRAC6, VM request.
    let hex = "415043500000003581000000010000000001000000";
    let mut reply: Vec<u8> = (0..hex.len()).step_by(2).map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap()).collect();
    reply.resize(53, 0);
    let s = proto::parse_apcp_reply(&reply).unwrap();
    assert_eq!(s.capabilities, proto::CAP_TCP_CLEARTEXT);
    assert_eq!(s.version_major, 1);
    assert_eq!(s.tcp_port, 0);
}

#[test]
fn map_packets() {
    let cd = proto::map_cd(1, 2048, 1000, &proto::ISO_TOC);
    assert_eq!(cd.len(), 44);
    assert_eq!(be32(&cd, 4), 44);
    assert_eq!(&cd[8..12], &[0x02, 0x10, 0, 1]);
    assert_eq!(be32(&cd, 12), 2048);
    assert_eq!(be32(&cd, 16), 1000);
    assert_eq!(cd[20], 1);
    assert_eq!(cd[21], 0xFF);
    assert_eq!(&cd[22..24], &[0, 20]);
    let fd = proto::map_disk(0, 512, 2880, false, 80, 2, 18);
    assert_eq!(fd.len(), 26);
    assert_eq!(&fd[22..26], &[0, 80, 2, 18]);
}

#[test]
fn decode_incoming() {
    let mut p = b"AVMP".to_vec();
    p.extend_from_slice(&22u32.to_be_bytes());
    p.extend_from_slice(&proto::ty::VDISK_INFO.to_be_bytes());
    p.extend_from_slice(&[0, 0, 0, 2, 1, 0, 0, 0, 2, 1, 0, 3]);
    match proto::decode(&p).unwrap() {
        Incoming::VDiskInfo(d) => {
            assert_eq!(d.len(), 2);
            assert_eq!(d[1], RemoteDrive { drive_type: 2, status: 1, capabilities: 3 });
        }
        o => panic!("{o:?}"),
    }
    let mut buf = proto::PacketBuf::default();
    buf.extend(&p[..5]);
    assert!(buf.next_packet().unwrap().is_none());
    buf.extend(&p[5..]);
    buf.extend(&p[..3]);
    assert_eq!(buf.next_packet().unwrap().unwrap(), p);
    assert!(buf.next_packet().unwrap().is_none());
}

#[test]
fn kvm_credentials() {
    let r = proto::kvm_credentials_request();
    assert_eq!(&r[..8], b"BEEF\x04\x27\x00\x10");
    let mut pl = vec![0u8; 64];
    pl[..3].copy_from_slice(b"abc");
    pl[32..36].copy_from_slice(b"wxyz");
    assert_eq!(proto::parse_kvm_credentials(&pl), Some(("abc".into(), "wxyz".into())));
}

fn avmp(ty: u16, body: &[u8]) -> Vec<u8> {
    let mut p = b"AVMP".to_vec();
    p.extend_from_slice(&((10 + body.len()) as u32).to_be_bytes());
    p.extend_from_slice(&ty.to_be_bytes());
    p.extend_from_slice(body);
    p
}

/// Full session against a fake iDRAC on loopback: setup, login, map an ISO,
/// serve a read split by the blocking factor, then a server disconnect.
#[test]
fn fake_idrac_session() {
    let dir = std::env::temp_dir().join(format!("idrac6-vm-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let iso = dir.join("t.iso");
    let mut data = vec![0u8; 2048 * 32];
    for (i, b) in data.iter_mut().enumerate() {
        *b = (i / 2048) as u8;
    }
    std::fs::write(&iso, &data).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = std::thread::spawn(move || {
        let (mut s, _) = listener.accept().unwrap();
        let mut req = [0u8; 53];
        s.read_exact(&mut req).unwrap();
        assert_eq!(&req[..4], b"APCP");
        assert_eq!(req[12], 2);
        let mut rep = vec![0u8; 53];
        rep[..4].copy_from_slice(b"APCP");
        rep[4..8].copy_from_slice(&53u32.to_be_bytes());
        rep[8] = 0x81;
        rep[12] = 1;
        rep[14..18].copy_from_slice(&1u32.to_be_bytes());
        s.write_all(&rep).unwrap();

        let login = read_packet_blocking(&mut s).unwrap();
        assert_eq!(login.len(), 153);
        assert_eq!(&login[13..15], b"vm");
        assert_eq!(&login[109..111], b"pw");
        let mut st = vec![0u8; 2 + 3 + 96];
        st[2] = 0;
        s.write_all(&avmp(proto::ty::LOGIN_STATUS, &st)).unwrap();

        let info = avmp(proto::ty::VDISK_INFO, &[0, 0, 0, 2, 1, 0, 0, 0, 2, 0, 0, 0]);
        // Initial GET_VDISK_INFO, then the one before the map.
        for _ in 0..2 {
            let g = read_packet_blocking(&mut s).unwrap();
            assert_eq!(proto::packet_type(&g), proto::ty::GET_VDISK_INFO);
            s.write_all(&info).unwrap();
        }
        let map = read_packet_blocking(&mut s).unwrap();
        assert_eq!(proto::packet_type(&map), proto::ty::VDISK_REQUEST);
        assert_eq!(&map[10..12], &[0, 1]); // second slot is the CD
        assert_eq!(be32(&map, 16), 31);

        // Read 5 blocks from block 4 with blocking factor 2 -> 3 chunks.
        let mut body = vec![0, 1];
        body.extend_from_slice(&4u32.to_be_bytes());
        body.extend_from_slice(&5u32.to_be_bytes());
        body.extend_from_slice(&2u32.to_be_bytes());
        s.write_all(&avmp(proto::ty::VDISK_READ, &body)).unwrap();
        let mut got = Vec::new();
        loop {
            let p = read_packet_blocking(&mut s).unwrap();
            match proto::packet_type(&p) {
                proto::ty::VDISK_READ_DATA => got.push(p),
                proto::ty::CLIENT_STATUS => {
                    assert_eq!(be32(&p, 12), 0);
                    break;
                }
                proto::ty::HEARTBEAT => {}
                t => panic!("unexpected {t}"),
            }
        }
        assert_eq!(got.len(), 3);
        assert_eq!(be32(&got[0], 12), 4);
        assert_eq!(be32(&got[2], 16), 1);
        assert_eq!(got[2].len(), 20 + 2048);
        assert_eq!(got[2][20], 8);
        assert_eq!(got[1][20 + 2048], 7);

        s.write_all(&avmp(proto::ty::DISCONNECT, &[0, 0, 1, 0, 0])).unwrap();
        // Client may release on its way out; drain until close.
        let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
        let mut sink = [0u8; 256];
        while let Ok(n) = s.read(&mut sink) {
            if n == 0 {
                break;
            }
        }
    });

    let (tx, rx) = mpsc::channel();
    let cfg = VmConfig {
        host: "127.0.0.1".into(),
        port,
        username: "vm".into(),
        password: "pw".into(),
        preempt: false,
    };
    let vm = VmSession::connect(cfg, tx).unwrap();
    vm.map(VmImage { path: iso.clone(), kind: DriveKind::Cd, read_only: true }).unwrap();
    let mut seen_connected = false;
    let mut seen_mapped = false;
    let reason = loop {
        match rx.recv_timeout(Duration::from_secs(10)).expect("event") {
            VmEvent::Connected { drives } => {
                assert_eq!(drives.len(), 2);
                seen_connected = true;
            }
            VmEvent::Mapped { kind, .. } => {
                assert_eq!(kind, DriveKind::Cd);
                seen_mapped = true;
            }
            VmEvent::Disconnected(r) => break r,
            _ => {}
        }
    };
    assert!(seen_connected && seen_mapped);
    assert_eq!(reason, "idle timeout");
    server.join().unwrap();
    drop(vm);
    let _ = std::fs::remove_dir_all(&dir);
}
