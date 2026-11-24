# rebacd, the pg_rebac daemon. The SQL (sql/rebac.sql) is installed separately, with psql or a migration.
#   docker build -t rebacd .
#   docker run -e DATABASE_URL=… -e SPICEDB_TOKEN=… -v $PWD/rebac.toml:/etc/rebac.toml rebacd --config /etc/rebac.toml run
FROM rust:1.94-slim-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
RUN cargo build --release --locked -p rebacd

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 rebac
COPY --from=build /src/target/release/rebacd /usr/local/bin/rebacd
USER rebac
ENTRYPOINT ["/usr/local/bin/rebacd"]
CMD ["run"]
