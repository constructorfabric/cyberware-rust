use toolkit_macros::temporary;

#[temporary(tracking = "gears-rust#1", reason = "valid", extra = "not allowed")]
pub struct X;

fn main() {}
