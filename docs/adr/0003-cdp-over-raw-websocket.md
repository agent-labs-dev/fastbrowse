# Rust speaks CDP over a raw websocket, not through a generated client

chromiumoxide produced a 2.0 MB binary and took about a minute to compile, because it decodes every CDP event
into one generated enum. A reader task on `tokio-tungstenite` that routes replies by id and events by method and
session is 1.3 MB with TLS and compiles in seconds. Commands and events stay JSON values, with typed structs only
where the code reads fields. Generated parameter types can be added later for the domains we use, as long as
routing stays on method names and only subscribed events are decoded.
