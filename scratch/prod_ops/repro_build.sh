#!/bin/bash
cd /e/RubiXDb
export RUSTFLAGS="-C link-arg=/Brepro --remap-path-prefix=E:\RubiXDb=/src --remap-path-prefix=C:\Users\Ruby\.cargo=/cargo"
for t in a b; do
  CARGO_TARGET_DIR=/e/rbx_repro_$t cargo build --release --locked -p rubixdb-cli -p rubixdb-api > scratch/prod_ops/repro_$t.log 2>&1
done
sha256sum /e/rbx_repro_a/release/rubixdb.exe /e/rbx_repro_b/release/rubixdb.exe /e/rbx_repro_a/release/rubixdb-api.exe /e/rbx_repro_b/release/rubixdb-api.exe > scratch/prod_ops/repro_hashes.txt
echo done >> scratch/prod_ops/repro_hashes.txt
