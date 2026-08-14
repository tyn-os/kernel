//! Host unit tests for the pure tmpfs path + membership + byte-cap core. Tests
//! live next to the code in src/tmpfs_tree.rs under #[cfg(test)]; this includes
//! them. `alloc` is declared inside the module itself.
#[path = "../../../src/tmpfs_tree.rs"]
mod tmpfs_tree;
