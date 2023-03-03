#![feature(test)]

extern crate test;

use test::Bencher;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap()
}

#[bench]
fn block_on(b: &mut Bencher) {
    let rt = rt();
    b.iter(|| rt.block_on(async {}));
}
