# Cross Compile for ARM (Raspberry Pi)

## openssl

build from source code

```
rustup target add armv7-unknown-linux-musleabihf # static link
sudo apt install gcc-arm-linux-gnueabihf
```
`armv7-unknown-linux-gnueabihf` for dynamic link

```
git clone https://github.com/openssl/openssl.git
./Configure linux-generic32 shared --cross-compile-prefix=arm-linux-gnueabihf-
make depend
make
make install
```

```
export OPENSSL_DIR=/usr/local
export TARGET=armv7-unknown-linux-musleabihf # Pi 2/3/4
cargo build --target $TARGET
```