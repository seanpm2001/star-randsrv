# Start by building the nitriding proxy daemon.
FROM golang:1.19 as go-builder

WORKDIR /src/
COPY . .
RUN make -C nitriding/cmd nitriding

# Build the web server application itself.
FROM rust:1.67.1 as rust-builder

WORKDIR /src/
COPY . .
# The '--locked' argument is important for reproducibility because it ensures
# that we use specific dependencies.
RUN cargo build --locked --release

# Copy from the builder imagse to keep the final image reproducible and small,
# and to improve reproducibilty of the build.
FROM alpine:3.17.2
COPY --from=go-builder /src/nitriding/cmd/nitriding /usr/local/bin/
COPY --from=rust-builder /src/target/release/star-randsrv /usr/local/bin/

# Set up the run-time environment
COPY start.sh /usr/local/bin/

EXPOSE 8443
# Switch to the UID that's typically reserved for the user "nobody".
USER 65534

CMD ["start.sh"]
