use toolkit_macros::temporary;

#[temporary(reason = "no tracking ref given")]
pub struct X;

fn main() {}
