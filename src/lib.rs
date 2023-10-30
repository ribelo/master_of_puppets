use std::any::Any;

// pub mod address;
pub mod address;
pub mod errors;
pub mod master;
pub mod message;
pub mod pid;
pub mod post_office;
pub mod praxis;
mod puppet;
pub mod supervision;

pub type BoxedAny = Box<dyn Any + Send + Sync>;
