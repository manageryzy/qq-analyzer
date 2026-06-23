use std::{env, fs};

use msg3_richtext_parser_rs::msg3_parser as parser;

fn main() -> anyhow::Result<()> {
    let path = env::args()
        .nth(1)
        .expect("usage: msg3_info_parse <info.bin>");
    let data = fs::read(path)?;
    println!("{}", parser::parse_info_json(&data));
    Ok(())
}
