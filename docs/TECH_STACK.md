# Tech Stack ARSY

## Rekomendasi

Bangun ARSY sebagai **Rust workspace dengan satu binary lokal**, SQLite untuk state, dan adapter HTTP langsung untuk provider. Pilihan ini memberi startup cepat, distribusi binary tunggal, kontrol process yang kuat, concurrency terstruktur, dan tipe yang membantu menjaga boundary tool/policy.

Jangan mulai dengan daemon, microservice, message broker, vector database, plugin marketplace, atau Kubernetes. Semuanya dapat ditambahkan setelah loop lokal aman dan terukur.

## Stack inti

| Area | Pilihan | Alasan |
| --- | --- | --- |
| Language | Rust stable | Binary tunggal, memory safety, async/process control, lint kuat. |
| Async runtime | Tokio | Ekosistem matang untuk streaming HTTP, process, cancellation, dan concurrency. |
| CLI | clap | Parser deklaratif dan help generation. |
| TUI | ratatui + crossterm | Cross-platform terminal UI tanpa frontend runtime terpisah. |
| Serialization | serde + serde_json + toml | Kontrak model/tool dan konfigurasi. |
| HTTP | reqwest + rustls | Streaming provider API tanpa OpenSSL system dependency. |
| Async trait | async-trait | Membuat adapter provider async tetap object-safe untuk runtime selection. |
| Persistence | SQLite + rusqlite | Lokal, transaksional, mudah dipaketkan; satu writer cukup untuk MVP. |
| Errors | thiserror; anyhow hanya di binary | Typed errors di library, context ergonomis di entrypoint. |
| Logging | tracing + tracing-subscriber | Structured logs dan correlation IDs. |
| Secrets | keyring | Delegasi ke OS keychain; fallback env untuk CI. |
| Hashing | sha2 | Content-addressed blobs dan evidence digest. |
| Tests | cargo test + assert_cmd | Unit/contract tests dan minimal CLI integration tests. |

Dependensi baru hanya ditambahkan saat modul yang membutuhkannya diimplementasikan. Tabel ini adalah target, bukan perintah untuk memasang semuanya di commit pertama.

## Kenapa bukan TypeScript atau Go?

| Opsi | Kekuatan | Biaya untuk ARSY |
| --- | --- | --- |
| TypeScript | Iterasi cepat dan SDK AI paling lengkap | Membutuhkan runtime, distribusi lebih berat, process isolation lebih mudah bocor lewat abstraction. |
| Go | Binary sederhana dan concurrency bagus | TUI/LLM ecosystem cukup baik, tetapi typed sum types dan error contracts kurang ekspresif. |
| Rust | Kontrol, performa, tipe, distribusi | Kurva belajar dan waktu compile lebih tinggi. |

Rust dipilih karena harness ini berada di security dan execution boundary. Jika tujuan berubah menjadi prototipe UI dua minggu, TypeScript lebih pragmatis; untuk produk CLI jangka panjang, Rust lebih sesuai.

## Kontrak provider

Gunakan adapter HTTP kecil terhadap API resmi, bukan framework agent umum. Core membutuhkan kontrak sempit:

```rust
#[async_trait]
trait ModelProvider {
    async fn capabilities(&self, model: &str) -> Result<ModelCapabilities>;
    async fn stream(
        &self,
        request: ModelRequest,
        sink: EventSink,
        cancel: CancellationToken,
    ) -> Result<ModelResponse>;
}
```

Adapter pertama:

1. OpenAI Responses API.
2. Anthropic Messages API.

Tambahkan provider ketiga hanya setelah contract test membuktikan dua adapter pertama tidak bocor ke core. Normalisasi bagian yang benar-benar umum; simpan fitur vendor-specific di `extensions` agar lowest-common-denominator tidak menghilangkan capability penting.

## Konfigurasi

Gunakan TOML berlapis:

```text
organization policy > CLI flags > project config > user config > defaults
```

Lokasi awal:

```text
~/.config/arsy/config.toml   # preferensi user
.arsy/config.toml            # konfigurasi repo tepercaya
.arsy/policy.toml            # policy repo; tidak boleh menurunkan policy organisasi
AGENTS.md                    # instruksi kerja
```

Credential disimpan di OS keychain atau environment CI. Nilai secret tidak didukung di project TOML.

## Penyimpanan lokal

SQLite MVP cukup dengan tabel berikut:

```sql
sessions(id, workspace, status, created_at, updated_at)
events(session_id, seq, kind, payload_json, created_at)
blobs(hash, media_type, byte_len, path)
approvals(id, session_id, scope_json, decision, expires_at)
```

Aktifkan WAL, foreign keys, dan busy timeout. Pertahankan satu writer task agar ordering event sederhana. Projection tambahan dibuat ketika query nyata membutuhkannya; jangan membuat ORM domain besar.

Blob berupa file content-addressed di data directory lokal. SQLite menyimpan metadata dan hash, bukan output command berukuran besar.

## Sandbox

Definisikan semantics lintas-platform lebih dulu:

- writable roots;
- readable roots;
- network mode;
- environment allowlist;
- process/time/output limits;
- child process cleanup.

Backend yang dievaluasi saat implementasi:

| Platform | Kandidat MVP |
| --- | --- |
| macOS | `sandbox-exec` bila tersedia, dengan guard filesystem di runtime sebagai defense-in-depth. |
| Linux | Landlock atau bubblewrap, dipilih lewat spike keamanan. |
| Windows | Job Objects dan restricted token/AppContainer pada fase terpisah. |

Tidak ada backend palsu. Jika sandbox yang diminta tidak tersedia, runtime gagal tertutup atau meminta pengguna memilih profile yang berbeda secara eksplisit.

## MCP dan ekstensi

Implementasikan MCP client setelah built-in tool contract stabil:

- STDIO transport lebih dulu.
- Streamable HTTP sesudahnya.
- JSON-RPC framing, initialize, list tools, call tool, cancellation, dan timeout.
- OAuth hanya ketika remote HTTP sudah dibutuhkan.

Skills awal cukup berupa directory dengan `SKILL.md` dan aset terkait. Hooks awal cukup executable yang menerima JSON event di stdin dan mengembalikan keputusan terstruktur. Plugin bundle dan marketplace ditunda.

## Observability

Gunakan `tracing` secara lokal dengan JSON opt-in. Tambahkan OpenTelemetry exporter hanya saat ada collector nyata; jangan menjadikannya dependency default MVP.

Metric wajib dihitung dari event yang sama dengan audit trail agar angka tidak berbeda:

- tokens dan estimasi biaya;
- latency provider/tool;
- approval outcome;
- retries dan cancellation;
- diff serta verification status.

## Struktur repo awal

Mulai lebih kecil daripada target arsitektur:

```text
Cargo.toml
crates/
  arsy-code/
  arsy-core/
  arsy-tui/
tests/
  fixtures/
```

`arsy-core` boleh memiliki module `models`, `policy`, `tools`, `runner`, dan `store`. Pecah menjadi crate sendiri hanya jika diperlukan oleh binary lain, compile boundary, atau ownership tim. Ini mencegah workspace besar sebelum kontraknya matang.

## Urutan implementasi

### Slice 1 — deterministic shell

- `arsy run` menerima prompt fixture.
- Event store, built-in read/search/shell tools, dan policy deny/allow.
- Fake provider untuk contract test.

Exit gate: fixture task selesai, event dapat direplay, dan deny terbukti tanpa side effect.

### Slice 2 — real model loop

- Satu provider adapter, streaming, tool call, budget, timeout, dan cancel.
- Patch tool serta verification evidence.

Exit gate: agent memperbaiki fixture repo secara end-to-end.

### Slice 3 — interactive product

- TUI, approval UX, session list/resume, compaction, dan cost summary.
- Provider adapter kedua.

Exit gate: kedua provider lulus suite kontrak dan sesi crash dapat dilanjutkan.

### Slice 4 — extension

- `AGENTS.md`, MCP STDIO, dan local skill.

Exit gate: tool eksternal tunduk pada policy yang sama dengan built-in tool.

### Slice 5 — orchestration

- Task DAG, reader subagent, cancellation, dan budget parent/child.
- Writer paralel/worktree hanya jika benchmark membuktikan kebutuhan.

Exit gate: suite multi-task menunjukkan hasil atau latency lebih baik daripada single-agent tanpa menurunkan safety.

## Quality gates

```console
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo deny check
```

Tambahkan `cargo-deny` ketika dependency pertama masuk. CI harus menguji Linux dan macOS; Windows masuk saat backend sandbox Windows mulai didukung.

Test minimum per boundary:

- provider fixture contract;
- policy deny/approval negative path;
- sandbox escape fixtures;
- event crash/replay;
- compaction recall;
- CLI exit code dan JSON output schema.

## Versioning dan kompatibilitas

- SemVer untuk CLI setelah rilis publik pertama.
- Schema version pada event, config, JSON output, skill manifest, dan runner protocol.
- Migrasi SQLite forward-only dengan backup otomatis sebelum perubahan.
- Feature flag untuk format eksperimental.
- Tidak ada stabilitas API yang dijanjikan sebelum schema contract tests tersedia.

## Yang sengaja tidak dipilih untuk MVP

- PostgreSQL, Redis, NATS, atau Kafka.
- Vector database dan embedding index permanen.
- Docker sebagai syarat menjalankan CLI.
- Web frontend atau Electron shell.
- General-purpose workflow engine.
- Agent framework besar yang mengambil alih control loop.

Tambahkan salah satu hanya setelah ada bottleneck atau requirement yang dapat diukur.
