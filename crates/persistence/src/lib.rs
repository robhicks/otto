//! Durable session store for the engine: persists sessions, their seq-ordered event
//! log, and turn records to sqlite, with gap-correct event replay. The `engine` layer
//! depends on this crate directly and holds a `Box<dyn SessionStore>`.
