// Test that #[temporary] accepts structs, enums, and impl blocks with valid
// `tracking`/`reason` arguments, and is a no-op at runtime.

use toolkit_macros::temporary;

#[temporary(
    tracking = "gears-rust#4347",
    reason = "in-memory stand-in until the real storage backend lands"
)]
pub struct InMemoryFooRepo;

#[temporary(tracking = "cargo-gears#89", reason = "placeholder pending upstream fix")]
pub enum FooKind {
    A,
    B,
}

pub struct Bar;

#[temporary(tracking = "gears-rust#1", reason = "temporary impl")]
impl Bar {
    fn noop(&self) {}
}

fn main() {
    let _ = InMemoryFooRepo;
    let _ = FooKind::A;
    Bar.noop();
}
