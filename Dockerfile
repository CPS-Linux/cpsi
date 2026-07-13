# syntax=docker/dockerfile:1

FROM rust:1.97.0-alpine3.24 AS builder

RUN apk add --no-cache build-base file

WORKDIR /build

COPY Cargo.toml Cargo.lock ./
COPY vendor/cps-common ./vendor/cps-common
COPY src ./src

RUN rustc -vV | grep -q 'host: .*unknown-linux-musl'
RUN cargo build --release --locked
RUN file target/release/cpsi | grep -Eq 'statically linked|static-pie linked'

FROM scratch AS artifact

COPY --from=builder /build/target/release/cpsi /cpsi
