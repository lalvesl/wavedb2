{
  description = "WaveDB — user-partitioned, tenant-centric embedded database";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
          config = {
            # MongoDB is SSPL (`meta.unfree = true`) and the benchmark's
            # reference peer (RFC 0060). Scoped to that one package on purpose:
            # a blanket `allowUnfree` would silently license anything a future
            # dependency drags in.
            #
            # `mongodb-ce`, not `mongodb`: unfree packages are not distributed
            # by cache.nixos.org, so the source attribute compiles the whole
            # server locally — hours of C++, more than the rest of the suite
            # combined, repeated at every version bump. `-ce` is the official
            # prebuilt tarball plus `autoPatchelf`, so the licence costs a
            # download instead. It ships `mongod`/`mongos` only; `mongosh` and
            # `mongoimport` come from their own (free) packages.
            allowUnfreePredicate =
              pkg: builtins.elem (nixpkgs.lib.getName pkg) [ "mongodb-ce" ];
          };
        };

        # Reads channel, components, and targets from rust-toolchain.toml.
        rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

        # Runtime libraries for wavedb-monitor-gui (eframe/winit links these
        # dynamically — without them the binary panics with NoWaylandLib).
        # Mirrors the sibling egui_shadcn flake's nativeLibs.
        guiLibs = with pkgs; [
          libxkbcommon
          libGL
          wayland
          libx11
          libxcursor
          libxrandr
          libxi
          fontconfig
        ];

        # Custom rust platform using the project toolchain (includes wasm32 target).
        rustPlatform = pkgs.makeRustPlatform {
          cargo = rustToolchain;
          rustc = rustToolchain;
        };

        # wasm-bindgen-cli built at the exact version used by the crate in Cargo.lock.
        wasmBindgenCli = pkgs.rustPlatform.buildRustPackage rec {
          pname = "wasm-bindgen-cli";
          version = "0.2.121";

          src = pkgs.fetchCrate {
            inherit pname version;
            hash = "sha256-ZOMgFNOcGkO66Jz/Z83eoIu+DIzo3Z/vq6Z5g6BDY/w=";
          };

          cargoHash = "sha256-DPdCDPTAPBrbqLUqnCwQu1dePs9lGg85JCJOCIr9qjU=";

          nativeBuildInputs = [ pkgs.pkg-config ];
          buildInputs = [
            pkgs.openssl
          ]
          ++ pkgs.lib.optionals pkgs.stdenv.isDarwin [
            pkgs.darwin.apple_sdk.frameworks.Security
          ];
        };
        # ── bench seeds: filled datasets as derivations (RFC 0060 §6) ──────────
        #
        # A filled data directory is a pure function of (system, version, rows,
        # seed), so it belongs in the Nix store rather than being refilled
        # before every run. Two properties are why this is Nix and not a
        # `~/.cache` directory:
        #
        #  * **Version binding is free and correct.** A data directory is locked
        #    to its server's major version; because the server package is an
        #    input, bumping it in `flake.lock` invalidates the seed. An ad-hoc
        #    cache would hand a PostgreSQL 18 datadir to PostgreSQL 19.
        #  * **Cached by inputs, not content.** These outputs hold timestamps,
        #    WAL and random system identifiers — they are *not* bit-reproducible
        #    and must never be marked fixed-output.
        #
        # `benchGen` is built once; `benchDataset` emits the portable TSV once;
        # every server-side seed loads that same file with its own bulk tool, so
        # a seed can exist before its Rust adapter does.
        benchGen = rustPlatform.buildRustPackage {
          pname = "bench-gen";
          version = "0.1.0";
          src = ./.;
          # The bench crate is outside the workspace, so it carries its own
          # lock — which is what makes this build hermetic.
          cargoLock.lockFile = ./benches/Cargo.lock;
          # Both are needed: `cargoRoot` says where the lock to vendor from
          # lives (the root one is the workspace's, a different dependency
          # set), `buildAndTestSubdir` says what to build.
          cargoRoot = "benches";
          buildAndTestSubdir = "benches";
          cargoBuildFlags = [
            "--bin"
            "bench-gen"
          ];
          # Without the `servers` feature. `bench-gen` fills WaveDB and writes a
          # TSV; it needs no database client, and it is a build input of every
          # seed, so compiling three drivers here would be paid on every seed
          # rebuild.
          buildNoDefaultFeatures = true;
          doCheck = false;
          nativeBuildInputs = [ pkgs.pkg-config ];
          # rusqlite links the pinned system SQLite, never its bundled copy.
          buildInputs = [ pkgs.sqlite ];
        };

        # The cage every measured run executes inside, so all five systems get
        # the *same* machine rather than whatever this one happens to be
        # (RFC 0060 §5). Three tools, because no one of them does all three
        # jobs:
        #
        #   systemd-run  the cgroup, and the only one of the three that can cap
        #                memory. `MemoryMax` bounds the **page cache** too,
        #                which is what stops a 2 GiB dataset from simply living
        #                in RAM and makes a cold read genuinely cold.
        #   taskset      the CPU budget. `AllowedCPUs` would be tidier, but
        #                `cpuset` is not among the controllers delegated to a
        #                user scope here (`cpu io memory pids`), so affinity it
        #                is — and affinity is what `nproc` reports, so the
        #                servers size their thread pools from it.
        #   bwrap        the namespace. It caps *nothing*: it is here for a
        #                private PID namespace, so a killed run cannot leave a
        #                mongod behind, and for one uniform filesystem shape.
        #
        # `--dev-bind / /` on purpose: the databases must write to the real
        # disk. A tmpfs would put them in RAM and measure the wrong thing.
        benchCpus = "0-3";
        benchCpuBudget = "4"; # how many `benchCpus` names, for the guard
        # 500 MB for the run AND its server, from the first instruction to the
        # last. Two reasons, and the second is the stronger one:
        #
        #   the measurement  at this size the page cache stops hiding the disk,
        #                    so a cold read is cold and a dataset larger than
        #                    memory is one (RFC 0060 open question 4);
        #   the comparison   Postgres, MySQL and MongoDB each size their caches
        #                    from the *machine's* RAM by default, so an uncaged
        #                    run does not compare five systems on one machine —
        #                    it compares five opinions about how much of the
        #                    machine to take. Each is pinned to 256 MB
        #                    (`benches/src/systems/server.rs`), MongoDB's floor
        #                    setting the number for all three.
        #
        # The scope is created AT the budget rather than loose-then-tightened:
        # a fill runs inside it too (it measured *faster* there — see
        # `benches/src/cage.rs`), so there was nothing left for the loose
        # window to buy, and one configuration with no exceptions is the whole
        # point. `benches/src/cage.rs` refuses to record outside it.
        benchMemMax = "524288000"; # 500 MB, in bytes for `memory.max`
        benchCage = ''
          export BENCH_MEM_MAX=${benchMemMax}
          export BENCH_CPU_BUDGET=${benchCpuBudget}
          exec systemd-run --user --scope -q \
            -p MemoryMax=${benchMemMax} -p MemorySwapMax=0 \
            -p Delegate=yes -- \
            taskset -c ${benchCpus} \
            bwrap --dev-bind / / --unshare-pid --proc /proc -- \
        '';

        # Default seed size. Deliberately modest: the exceeds-RAM sizes of
        # RFC 0060 §3 take hours to fill through a per-op-fsync engine, which is
        # open question 4 in that RFC and is not answered here.
        benchRows = 200000;
        benchSeed = 42;
        rows = toString benchRows;
        sd = toString benchSeed;

        # The dataset, once, in the one portable form every bulk loader reads.
        benchDataset =
          pkgs.runCommand "bench-dataset-${rows}" { nativeBuildInputs = [ benchGen ]; }
            ''
              mkdir -p "$out"
              bench-gen emit-tsv --rows ${rows} --seed ${sd} \
                --out "$out/dataset.tsv"
              cat > "$out/manifest.txt" <<EOF
              rows=${rows}
              seed=${sd}
              columns=id,kind,score,name,tag,body
              EOF
            '';

        # Every seed is a data directory a server can be pointed at directly.
        # Two rules hold in all of them: the server is shut down **cleanly**
        # inside the builder (otherwise the first measured operation pays for
        # crash recovery), and nothing is timed.
        mkSeed =
          name: deps: script:
          pkgs.runCommand "bench-seed-${name}-${rows}"
            {
              nativeBuildInputs = deps;
              dataset = benchDataset;
            }
            ''
              mkdir -p "$out"
              # `cat`, not `cp`: a copy out of the store keeps its read-only
              # mode and the append below would fail.
              cat "$dataset/manifest.txt" > "$out/manifest.txt"
              echo "system=${name}" >> "$out/manifest.txt"
              export HOME="$TMPDIR"

              # Every seed must prove it loaded. Bulk loaders are cheerful: a
              # `psql` heredoc without ON_ERROR_STOP returns 0 after a failed
              # \copy, which once produced a green build and an empty database.
              # A seed that builds but holds nothing is worse than no seed —
              # the benchmark would measure an empty table and report it.
              expectRows() {
                if [ "$1" != "${rows}" ]; then
                  echo "seed ${name}: loaded $1 rows, expected ${rows}" >&2
                  exit 1
                fi
                echo "seed ${name}: verified $1 rows"
              }

              ${script}
            '';

        benchSeeds = {
          # WaveDB has no bulk path — one insert is one batch is one fsync — so
          # it fills through the engine rather than from the TSV. The sidecar
          # (`ids.bin`, `pivot.bin`) is not optional: a NonUnique anchor id is
          # minted from the clock and cannot be recomputed from the seed.
          wavedb = mkSeed "wavedb" [ benchGen ] ''
            bench-gen fill-wavedb --rows ${rows} --seed ${sd} --out "$out"
            # 16 bytes per minted anchor id — the sidecar's length is the
            # record count, and without it the seed is unreadable.
            expectRows "$(( $(stat -c %s "$out/ids.bin") / 16 ))"
          '';

          sqlite = mkSeed "sqlite" [ pkgs.sqlite ] ''
            sqlite3 "$out/bench.db" <<SQL
            .bail on
            CREATE TABLE thing (
              id    INTEGER PRIMARY KEY,
              kind  INTEGER NOT NULL,
              score INTEGER NOT NULL,
              name  TEXT    NOT NULL,
              tag   TEXT    NOT NULL,
              body  TEXT    NOT NULL
            );
            .mode tabs
            .import $dataset/dataset.tsv thing
            CREATE INDEX idx_thing_tag ON thing(tag);
            PRAGMA journal_mode = WAL;
            PRAGMA wal_checkpoint(TRUNCATE);
            SQL
            expectRows "$(sqlite3 "$out/bench.db" 'SELECT count(*) FROM thing')"
          '';

          postgres = mkSeed "postgres" [ pkgs.postgresql_18 ] ''
            initdb -D "$out/data" --no-locale --encoding=UTF8 -U bench
            pg_ctl -D "$out/data" -w -o "-k $TMPDIR -c listen_addresses=" start
            psql -h "$TMPDIR" -U bench -d postgres -v ON_ERROR_STOP=1 <<SQL
            CREATE TABLE thing (
              id    BIGINT PRIMARY KEY,
              kind  INTEGER NOT NULL,
              score BIGINT  NOT NULL,
              name  TEXT    NOT NULL,
              tag   TEXT    NOT NULL,
              body  TEXT    NOT NULL
            );
            \copy thing FROM '$dataset/dataset.tsv' WITH (FORMAT text)
            CREATE INDEX idx_thing_tag ON thing(tag);
            CHECKPOINT;
            SQL
            expectRows "$(psql -h "$TMPDIR" -U bench -d postgres -tAc \
              'SELECT count(*) FROM thing')"
            pg_ctl -D "$out/data" -w stop   # clean shutdown: no recovery later
          '';

          mysql = mkSeed "mysql" [ pkgs.mysql84 ] ''
            mysqld --initialize-insecure --datadir="$out/data" \
              --user="$(id -un)" --log-error="$TMPDIR/init.log"
            mysqld --datadir="$out/data" --socket="$TMPDIR/mysql.sock" \
              --skip-networking --log-error="$TMPDIR/run.log" \
              --local-infile=1 &
            for _ in $(seq 1 60); do
              mysqladmin --socket="$TMPDIR/mysql.sock" -u root ping \
                >/dev/null 2>&1 && break
              sleep 1
            done
            mysql --socket="$TMPDIR/mysql.sock" -u root --local-infile=1 <<SQL
            CREATE DATABASE bench;
            USE bench;
            CREATE TABLE thing (
              id    BIGINT PRIMARY KEY,
              kind  INT    NOT NULL,
              score BIGINT NOT NULL,
              name  TEXT   NOT NULL,
              tag   VARCHAR(64) NOT NULL,
              body  TEXT   NOT NULL
            ) ENGINE=InnoDB;
            LOAD DATA LOCAL INFILE '$dataset/dataset.tsv' INTO TABLE thing;
            CREATE INDEX idx_thing_tag ON thing(tag);
            SQL
            expectRows "$(mysql --socket="$TMPDIR/mysql.sock" -u root -N -B \
              -e 'SELECT count(*) FROM bench.thing')"
            mysqladmin --socket="$TMPDIR/mysql.sock" -u root shutdown
            wait
          '';

          # The one seed that cannot run directly in the Nix sandbox. MongoDB's
          # bundled tcmalloc reads the possible-CPU mask at startup and
          # `CHECK`-fails (SIGABRT, before a single argument is parsed) when it
          # cannot — and the sandbox neither mounts /sys nor lets the builder
          # create it, since its root is read-only. So the whole seed runs
          # inside a bubblewrap namespace carrying a synthetic /sys. The value
          # is **fixed**: the seed must stay a pure function of its inputs, not
          # of the builder's core count.
          mongodb = mkSeed "mongodb" [
            pkgs.mongodb-ce # mongod
            pkgs.mongosh # the shell (free, separate package)
            pkgs.mongodb-tools # mongoimport (free, separate package)
            pkgs.bubblewrap # the /sys shim below
            pkgs.bash # bwrap's root is a fresh tmpfs: no /bin/sh in it
          ] ''
            echo 0-3 > "$TMPDIR/cpu-possible"
            mkdir -p "$out/data"

            # Quoted heredoc: this runs *inside* the namespace and reads its
            # paths from the environment bwrap forwards below.
            cat > "$TMPDIR/seed.sh" <<'SEED'
            set -euo pipefail
            mongod --dbpath "$OUT/data" --bind_ip 127.0.0.1 --port 27017 \
              --fork --logpath "$TMPDIR/mongod.log"
            # `--columnsHaveTypes` needs the types spelled out, or every column
            # arrives as a string and the collection silently holds the wrong
            # shape. `_id` is set from the dataset id so the point-lookup key is
            # the same value on every system.
            mongoimport --host 127.0.0.1 --port 27017 \
              --db bench --collection thing --type tsv \
              --columnsHaveTypes \
              --fields '_id.int64(),kind.int32(),score.int64(),name.string(),tag.string(),body.string()' \
              --file "$DATASET/dataset.tsv" \
              --numInsertionWorkers 4
            mongosh --host 127.0.0.1 --port 27017 bench --quiet --eval \
              'db.thing.createIndex({tag: 1})'
            mongosh --host 127.0.0.1 --port 27017 bench --quiet \
              --eval 'db.thing.countDocuments({})' > "$TMPDIR/count.txt"
            mongod --dbpath "$OUT/data" --shutdown
            SEED

            bwrap \
              --tmpfs / \
              --ro-bind /nix/store /nix/store \
              --bind "$TMPDIR" "$TMPDIR" \
              --bind "$out" "$out" \
              --proc /proc --dev /dev --tmpfs /tmp \
              --tmpfs /sys \
              --ro-bind "$TMPDIR/cpu-possible" /sys/devices/system/cpu/possible \
              --setenv HOME "$TMPDIR" \
              --setenv OUT "$out" \
              --setenv DATASET "$dataset" \
              --chdir "$TMPDIR" \
              -- bash "$TMPDIR/seed.sh"

            # Counted inside the namespace, verified out here: `expectRows` is
            # the outer builder's function.
            expectRows "$(cat "$TMPDIR/count.txt")"
          '';
        };

      in
      {
        packages.wasm = rustPlatform.buildRustPackage {
          pname = "wavedb-wasm";
          version = "0.1.0";
          src = ./.;

          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = [
            wasmBindgenCli
            pkgs.binaryen # wasm-opt
            pkgs.gzip
          ];

          # The exported `example_roundtrip` entry point (src/example.rs)
          # exercises the engine — schema macro, migration chain, query DSL,
          # IndexedDB — so fat LTO keeps the codebase and the size below is a
          # meaningful number, not an empty shell.
          buildPhase = ''
            runHook preBuild
            cargo build --target wasm32-unknown-unknown --profile wasm-release -p wavedb-wasm
            runHook postBuild
          '';

          installPhase = ''
            runHook preInstall
            mkdir -p $out
            wasm-bindgen \
              --out-dir $out \
              --target bundler \
              target/wasm32-unknown-unknown/wasm-release/wavedb_wasm.wasm

            # Post-link size pass.  Feature flags match what rustc 1.8x+
            # emits and wasm-bindgen's externref pass requires.
            for f in $out/*_bg.wasm; do
              before=$(stat -c%s "$f")
              wasm-opt -Oz \
                --enable-bulk-memory \
                --enable-sign-ext \
                --enable-mutable-globals \
                --enable-nontrapping-float-to-int \
                --enable-reference-types \
                "$f" -o "$f.opt"
              mv "$f.opt" "$f"
              after=$(stat -c%s "$f")
              gzipped=$(gzip -9 -c "$f" | wc -c)
              echo "wasm size: $f  raw=$after (was $before)  gzip=$gzipped"
            done
            runHook postInstall
          '';

          doCheck = false;
        };

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            pkg-config
            rustToolchain

            # Code quality
            cargo-mutants
            cargo-deny
            taplo
            nixpkgs-fmt
            prettier

            # Testing
            cargo-nextest

            # WASM
            wasm-pack
            wasm-bindgen-cli
          ];

          buildInputs =
            with pkgs;
            [
              openssl
              # The benchmark's SQLite peer (RFC 0060) links the system
              # library, never `rusqlite`'s bundled copy — the version has to
              # come from `flake.lock` like every other measured system.
              sqlite
            ]
            ++ lib.optionals stdenv.isLinux guiLibs
            ++ lib.optionals stdenv.isDarwin [
              darwin.apple_sdk.frameworks.SystemConfiguration
              darwin.apple_sdk.frameworks.CoreFoundation
              darwin.apple_sdk.frameworks.Security
            ];

          shellHook = ''
            export PKG_CONFIG_PATH="${pkgs.openssl.dev}/lib/pkgconfig:${pkgs.sqlite.dev}/lib/pkgconfig"
            ${pkgs.lib.optionalString pkgs.stdenv.isLinux ''
              export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath guiLibs}:$LD_LIBRARY_PATH"
            ''}
          '';
        };

        # ── bench: the comparative benchmark suite (RFC 0060) ──────────────────
        #
        # Both brackets: the WaveDB engine in-process against SQLite, and
        # MongoDB / PostgreSQL / MySQL over a local connection. Every competitor
        # version comes from `flake.lock` exactly like the toolchain does — that
        # pinning is the reason this runs through Nix at all, and it is what
        # makes a recorded result attributable to a whole stack.
        #
        # The server binaries are `runtimeInputs` rather than an assumption
        # about the machine: an adapter starts its own server in the run's
        # scratch directory, so a benchmark row can never be measuring whatever
        # the developer happens to have installed and running.
        #
        # `benches/` is outside the cargo workspace on purpose (the drivers must
        # not reach the shipped dependency graph), so this builds it in place.
        apps.bench = {
          type = "app";
          program = "${
            pkgs.writeShellApplication {
              name = "bench";
              runtimeInputs = with pkgs; [
                rustToolchain
                pkg-config
                sqlite
                git
                postgresql_18 # postgres, initdb, pg_ctl
                mysql84 # mysqld, mysqladmin
                mongodb-ce # mongod
                bubblewrap # the sandbox
                util-linux # taskset
                systemd # systemd-run, for the cgroup
              ];
              text = ''
                set -euo pipefail
                repo="$(git rev-parse --show-toplevel)"
                export PKG_CONFIG_PATH="${pkgs.sqlite.dev}/lib/pkgconfig"
                cargo build --release --manifest-path "$repo/benches/Cargo.toml"
                ${benchCage} "$repo/benches/target/release/wavedb-bench" \
                  --repo "$repo" "$@"
              '';
            }
          }/bin/bench";
        };

        # `nix build .#bench-seed-postgres` etc. Keep the result symlinks as GC
        # roots, or a `nix-collect-garbage` between runs eats the fill.
        packages.bench-gen = benchGen;
        packages.bench-dataset = benchDataset;
        packages.bench-seed-wavedb = benchSeeds.wavedb;
        packages.bench-seed-sqlite = benchSeeds.sqlite;
        packages.bench-seed-postgres = benchSeeds.postgres;
        packages.bench-seed-mysql = benchSeeds.mysql;
        packages.bench-seed-mongodb = benchSeeds.mongodb;

        # Materialise every seed and run against them, so a repeat run skips the
        # fill entirely. Building this app builds all five seeds, which is the
        # point: they are inputs, not a side effect.
        apps.bench-seeded = {
          type = "app";
          program = "${
            pkgs.writeShellApplication {
              name = "bench-seeded";
              runtimeInputs = with pkgs; [
                rustToolchain
                pkg-config
                sqlite
                git
                coreutils
                postgresql_18
                mysql84
                mongodb-ce
                bubblewrap
                util-linux
                systemd
              ];
              text = ''
                set -euo pipefail
                repo="$(git rev-parse --show-toplevel)"
                export PKG_CONFIG_PATH="${pkgs.sqlite.dev}/lib/pkgconfig"
                export BENCH_SEED_WAVEDB="${benchSeeds.wavedb}"
                export BENCH_SEED_SQLITE="${benchSeeds.sqlite}"
                export BENCH_SEED_POSTGRES="${benchSeeds.postgres}"
                export BENCH_SEED_MYSQL="${benchSeeds.mysql}"
                export BENCH_SEED_MONGODB="${benchSeeds.mongodb}"
                cargo build --release --manifest-path "$repo/benches/Cargo.toml"
                ${benchCage} "$repo/benches/target/release/wavedb-bench" \
                  --repo "$repo" --rows ${rows} "$@"
              '';
            }
          }/bin/bench-seeded";
        };

        apps.wavedb_monitor = {
          type = "app";
          program = "${
            pkgs.writeShellApplication {
              name = "wavedb-monitor";
              runtimeInputs = [ rustToolchain ];
              text = ''
                cargo run --release --bin wavedb-monitor "$@"
              '';
            }
          }/bin/wavedb-monitor";
        };

        # ── real_example: multi-process orchestrated load test ──────────────────
        #
        # Builds all five binaries first, then runs the orchestrator which
        # spawns subprocesses pointing at the sibling binaries in the same
        # release output directory.
        apps.real_example = {
          type = "app";
          program = "${
            pkgs.writeShellApplication {
              name = "real_example";
              runtimeInputs = [ rustToolchain ];
              text = ''
                set -euo pipefail

                echo "── Building all real_example binaries ──────────────────────────"
                cargo build --release \
                  --bin real_example \
                  --bin re_slow_node \
                  --bin re_quick_node \
                  --bin re_client \
                  --bin re_monitor

                echo "── Launching orchestrator ──────────────────────────────────────"
                # The orchestrator discovers its sibling binaries via
                # std::env::current_exe() — all five binaries live in the same
                # target/release/ directory after the cargo build above.
                exec ./target/release/real_example "$@"
              '';
            }
          }/bin/real_example";
        };

        # ── real_example_gui: the load test, monitored by the desktop GUI ───────
        #
        # Same 500-client payment-gateway scenario as real_example, but the
        # monitor is the egui desktop GUI opened on the Data tab — watch the
        # record graph, throughput, and page maps move live under load. Close
        # the GUI window to stop the scenario.
        #
        #   nix run .#real_example_gui
        apps.real_example_gui = {
          type = "app";
          program = "${
            pkgs.writeShellApplication {
              name = "real_example_gui";
              runtimeInputs = [
                rustToolchain
                pkgs.coreutils
              ];
              text = ''
                set -euo pipefail

                echo "── Building real_example + GUI binaries ─────────────────────────"
                cargo build --release \
                  --bin real_example \
                  --bin re_slow_node \
                  --bin re_quick_node \
                  --bin re_client \
                  --bin wavedb-monitor-gui

                echo "── Launching orchestrator with the GUI monitor ──────────────────"
                # eframe links wayland/libGL/etc. dynamically — put them on the
                # loader path for the GUI child the orchestrator spawns.
                export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath guiLibs}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
                export WAVE_MONITOR=gui
                exec ./target/release/real_example "$@"
              '';
            }
          }/bin/real_example_gui";
        };

        # ── monitor_gui_demo: turnkey GUI monitor against a live cluster ─────────
        #
        # One command: build the binaries, start a keyed 3-node cluster in a
        # temp dir, seed three tenants, then open the GUI pointed at them with
        # the cluster key. Closing the GUI window tears the whole thing down
        # (the EXIT trap kills the nodes and removes the temp dir).
        #
        #   nix run .#monitor_gui_demo
        #   nix run .#monitor_gui_demo -- --tab data   # extra GUI flags pass through
        #
        # Uses fixed ports 7700/7701/7800 — stop any other cluster on those
        # ports first.
        apps.monitor_gui_demo = {
          type = "app";
          program = "${
            pkgs.writeShellApplication {
              name = "monitor_gui_demo";
              runtimeInputs = [
                rustToolchain
                pkgs.coreutils
              ];
              text = ''
                set -euo pipefail

                # Demo cluster secret (32 bytes / 64 hex). Node-to-node + the
                # monitor's HMAC tokens use this; clients write without it.
                KEY=000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f

                echo "── Building demo binaries (release) ──────────────────────────────"
                cargo build --release \
                  --bin wavedb-slow-node \
                  --bin wavedb-quick-node \
                  --bin wavedb-monitor-gui \
                  --bin re_client

                bin=./target/release
                data="$(mktemp -d /tmp/wavedb-gui-demo.XXXXXX)"
                pids=()

                cleanup() {
                  echo
                  echo "── Stopping demo cluster ───────────────────────────────────────"
                  for pid in "''${pids[@]}"; do kill "$pid" 2>/dev/null || true; done
                  rm -rf "$data"
                }
                trap cleanup EXIT INT TERM

                echo "── Starting keyed cluster (2 quick + 1 slow) in $data ──"
                "$bin/wavedb-slow-node" --listen 127.0.0.1:7800 \
                  --data-dir "$data/slow" --cluster-key "$KEY" \
                  >"$data/slow.log" 2>&1 &
                pids+=("$!")
                sleep 1
                "$bin/wavedb-quick-node" --listen 127.0.0.1:7700 \
                  --peers 127.0.0.1:7701 --slow-node 127.0.0.1:7800 \
                  --data-dir "$data/q0" --cluster-key "$KEY" \
                  >"$data/q0.log" 2>&1 &
                pids+=("$!")
                "$bin/wavedb-quick-node" --listen 127.0.0.1:7701 \
                  --peers 127.0.0.1:7700 --slow-node 127.0.0.1:7800 \
                  --data-dir "$data/q1" --cluster-key "$KEY" \
                  >"$data/q1.log" 2>&1 &
                pids+=("$!")
                sleep 2

                echo "── Seeding tenants 42, 77, 1001 (writes over WebSocket) ──"
                for tenant in 42 77 1001; do
                  WAVE_QN_WS_URLS="ws://127.0.0.1:7700/ws,ws://127.0.0.1:7701/ws" \
                  WAVE_TENANT="$tenant" WAVE_CLIENT_ID=0 WAVE_NUM_CLIENTS=1 \
                    timeout 3 "$bin/re_client" >/dev/null 2>&1 || true
                done

                echo "── Waiting for the first history flush to the slow node ──"
                sleep 6

                echo "── Launching GUI — close the window to stop the demo ──"
                export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath guiLibs}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
                "$bin/wavedb-monitor-gui" \
                  --quick-nodes http://127.0.0.1:7700,http://127.0.0.1:7701 \
                  --slow-nodes http://127.0.0.1:7800 \
                  --cluster-key "$KEY" "$@"
              '';
            }
          }/bin/monitor_gui_demo";
        };

        apps.fmt = {
          type = "app";
          program = "${
            pkgs.writeShellApplication {
              name = "fmt";
              runtimeInputs = with pkgs; [
                rustToolchain
                nixfmt
                taplo
                prettier
                jq
              ];
              text = ''
                nixfmt .
                cargo fmt --all
                taplo fmt
                prettier --write "**/*.md"
                while IFS= read -r -d "" f; do
                  tmp="$(mktemp)"
                  jq . "$f" > "$tmp" && mv "$tmp" "$f"
                done < <(find . -name "*.jsonl" -not -path "./.git/*" -not -path "./target/*" -print0)
              '';
            }
          }/bin/fmt";
        };
      }
    );
}
