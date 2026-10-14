// A permit for healthy A cannot be retargeted by swapping A with writer B.
use stratava_core::{StratavaStreamWriter, enforcement_probe::borrow_stream_writer};
fn cannot_swap(a: &mut StratavaStreamWriter, b: &mut StratavaStreamWriter) {
    let permit = borrow_stream_writer(a).unwrap();
    std::mem::swap(a, b);
    let _window = permit.begin_mutation();
}
fn main() {}
