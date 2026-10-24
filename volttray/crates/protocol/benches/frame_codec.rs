//! E20（红蓝对抗终审 P2b 批，2026-10-24）：纯逻辑链微基准①——
//! HID++ 帧编解码（Frame::encode / Frame::decode roundtrip）。
//! 只测纯计算路径，零 I/O、零硬件；用途是演进期的性能回归观察
//! （首版只跑数不设阈值门禁，见 ci.yml microbench job 注释）。

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use volttray_protocol::frame::Frame;

fn bench_frame_codec(c: &mut Criterion) {
    let mut g = c.benchmark_group("frame_codec");

    // 长帧（report 0x11，16 参）组帧 + 解帧 roundtrip——协议最热路径形态
    g.bench_function("long_frame_encode_decode_roundtrip", |b| {
        b.iter(|| {
            let f = Frame::new_long(
                black_box(0x01),
                black_box(0x07),
                0x02,
                0x05,
                black_box([0xAB; 16]),
            );
            let bytes = f.encode();
            Frame::decode(black_box(&bytes)).expect("自产帧必须可解码")
        })
    });

    // 短帧（report 0x10，3 参）roundtrip
    g.bench_function("short_frame_encode_decode_roundtrip", |b| {
        b.iter(|| {
            let f = Frame::new_short(
                black_box(0x02),
                black_box(0x03),
                0x01,
                0x04,
                black_box([0x55; 3]),
            );
            let bytes = f.encode();
            Frame::decode(black_box(&bytes)).expect("自产帧必须可解码")
        })
    });

    // 纯解码（固定线上字节流）——接收方向热路径
    let raw_long: Vec<u8> = {
        let f = Frame::new_long(0x01, 0x07, 0x02, 0x05, [0xAB; 16]);
        f.encode()
    };
    g.bench_function("long_frame_decode_only", |b| {
        b.iter(|| Frame::decode(black_box(&raw_long)).expect("固定合法帧必须可解码"))
    });

    g.finish();
}

criterion_group!(benches, bench_frame_codec);
criterion_main!(benches);
