# FEC reference vectors

`main.c` encodes fixed patterns with [nanors](https://github.com/sleepybishop/nanors),
the Reed–Solomon library Sunshine/Hermes use for video FEC, and prints the
parity shards. Its output is embedded in `crates/hestia-stream/src/fec.rs`
(the `matches_nanors` test), so the Rust encoder/decoder is checked byte for
byte against the host's.

```sh
git clone --depth 1 https://github.com/sleepybishop/nanors
gcc -O2 -Inanors -Inanors/deps/obl main.c nanors/rs.c \
    nanors/deps/obl/oblas_lite.c -o nanors_vec && ./nanors_vec
```
