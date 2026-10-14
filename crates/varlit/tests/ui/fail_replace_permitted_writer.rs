// A permit for healthy A cannot be retargeted by swapping A with writer B.
use varlit_core::{VarlitStreamWriter, enforcement_probe::borrow_stream_writer};
fn cannot_swap(a: &mut VarlitStreamWriter, b: &mut VarlitStreamWriter) {
    let permit = borrow_stream_writer(a).unwrap();
    std::mem::swap(a, b);
    let _window = permit.begin_mutation();
}
fn main() {}
