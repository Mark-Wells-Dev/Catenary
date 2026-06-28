//! Minimal fixture crate for eyeballing `catenary grep` / `catenary glob`
//! against real rust-analyzer (ticket 00c). It deliberately packs a base trait,
//! a subtrait, two structs implementing both, and a small call graph so that
//! `impl` / `super` / `sub` / `caller` / `callee` edges all have something to
//! surface.

/// Base trait: anything that has a name and a leg count.
///
/// Implemented by [`Dog`] and [`Cat`]; supertrait of [`Pet`].
pub trait Animal {
    /// The animal's name.
    fn name(&self) -> String;

    /// Number of legs (defaulted, overridable).
    fn legs(&self) -> u32 {
        4
    }
}

/// Subtrait of [`Animal`]: an animal that can also greet.
///
/// `Pet: Animal` makes `Animal` a supertype and `Pet` a subtype in the type
/// hierarchy rust-analyzer reports.
pub trait Pet: Animal {
    /// A spoken greeting.
    fn greet(&self) -> String;
}

/// A dog. Implements both [`Animal`] and [`Pet`].
pub struct Dog {
    /// The name the dog answers to.
    pub call_sign: String,
}

/// A cat. Implements both [`Animal`] and [`Pet`].
pub struct Cat;

impl Animal for Dog {
    fn name(&self) -> String {
        self.call_sign.clone()
    }
}

impl Pet for Dog {
    fn greet(&self) -> String {
        format!("{} says woof", self.name())
    }
}

impl Animal for Cat {
    fn name(&self) -> String {
        "cat".to_string()
    }

    fn legs(&self) -> u32 {
        4
    }
}

impl Pet for Cat {
    fn greet(&self) -> String {
        format!("{} says meow", self.name())
    }
}

/// Describe any animal — calls [`Animal::name`] and [`Animal::legs`].
pub fn describe(animal: &dyn Animal) -> String {
    format!("{} has {} legs", animal.name(), animal.legs())
}

/// Greet a pet, then describe it. Exercises a two-hop call graph:
/// `introduce` -> `describe_pet` -> `describe`, and `introduce` -> `Pet::greet`.
pub fn introduce(pet: &dyn Pet) -> String {
    let greeting = pet.greet();
    let body = describe_pet(pet);
    format!("{greeting}; {body}")
}

/// Forward a pet to [`describe`] as a plain animal.
fn describe_pet(pet: &dyn Pet) -> String {
    describe(pet)
}

/// Build the two sample pets and introduce each — top-level entry point so the
/// call graph has a clear root.
pub fn run() -> Vec<String> {
    let dog = Dog {
        call_sign: "rex".to_string(),
    };
    let cat = Cat;
    vec![introduce(&dog), introduce(&cat)]
}
