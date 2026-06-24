//! otto retrieval: a persistent, stat-incremental inverted index behind the `Retriever` seam.
//! The index lives in a standalone sqlite DB (owned here, separate from the session store) and
//! scores content for every indexed file, removing the ContextFinder's per-turn read budget.

mod chunk;
mod index;
mod retriever;
mod tokenize;
mod walk;

pub use retriever::IndexedRetriever;
