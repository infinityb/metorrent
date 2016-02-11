#![feature(plugin)]
#![plugin(afl_plugin)]

extern crate afl;
extern crate bencode;

use std::io::{self, Read};
use bencode::{from_slice, Value};

fn main() {
    let mut buf = Vec::new();
    if io::stdin().take(1 << 20).read_to_end(&mut buf).is_err() {
        return;
    }

    match from_slice::<Value>(&*buf) {
        Ok(bencode) => println!("{:#?}", bencode),
	Err(err) => println!("erorr: {:?}", err),
    }
}
