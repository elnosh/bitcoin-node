# bitcoin-node

a tiny bitcoin node using [rust-bitcoinkernel](https://github.com/TheCharlatan/rust-bitcoinkernel) which is a wrapper around [libbitcoinkernel](https://github.com/bitcoin/bitcoin/issues/27587).


### Usage

run on regtest:
```
cargo run --bin btc-node -- --network regtest
```

there's also a CLI tool with 2 commands: `getblockcount` and `getblock`

- `getblockcount` similar to `bitcoin-cli getblockcount` returns the height of current active chain

```
cargo run --bin bcli getblockcount
```


- `getblock` returns a block at a certain height:
```
cargo run --bin bcli getblock 21
```
