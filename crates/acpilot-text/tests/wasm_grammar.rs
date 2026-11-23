use std::sync::Arc;

use acpilot_text::highlight::token_for;
use acpilot_text::{Buffer, Grammar, Highlights};

const WASM: &[u8] = include_bytes!("fixtures/simple/simple.wasm");
const HIGHLIGHTS: &str = include_str!("fixtures/simple/queries/highlights.scm");

#[test]
fn a_wasm_grammar_parses_and_paints() {
    let grammar =
        Grammar::from_wasm("simple", WASM, HIGHLIGHTS).expect("the fixture grammar loads");
    let mut highlights = Highlights::new(Arc::new(grammar));
    let buffer = Buffer::from_text("alpha 42");

    assert!(highlights.sync(&buffer));
    let spans = highlights.spans(&buffer, 0..buffer.char_len());
    assert_eq!(spans.len(), 2, "the word and the number are painted");
    assert_eq!(spans[0].range, 0..5);
    assert_eq!(spans[0].token, token_for("variable").expect("variable"));
    assert_eq!(spans[1].range, 6..8);
    assert_eq!(spans[1].token, token_for("number").expect("number"));
}
