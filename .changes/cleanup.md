---
"snapshot": patch
"iota-stronghold": patch
"vault": patch
"communication-macros": patch
"stronghold-communication": patch
"commandline": patch
---

Updated cargo.toml files with the updated crypto.rs revisions and authors. 
Fixed logic in snapshot and providers to use the `try_*` encryption and decryption functions.
Fixed commandline and stopped it from overwriting snapshots. 

