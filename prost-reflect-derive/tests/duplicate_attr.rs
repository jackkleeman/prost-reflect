use prost_reflect_derive::ReflectMessage;

#[derive(ReflectMessage)]
#[prost_reflect(file_descriptor_path = b"123", message_name = "msg")]
#[prost_reflect(file_descriptor_path = b"123", message_name = "msg")]
pub struct MyMessage {}

fn main() {}
