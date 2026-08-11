# Dokumen Produk ARSY

## Ringkasan

ARSY adalah harness agent untuk software engineering: lapisan runtime yang menghubungkan pengguna, model, konteks repo, tools, policy, dan eksekutor. Fokusnya bukan membuat model baru, tetapi membuat kerja model yang ada menjadi aman, dapat dilanjutkan, dapat dipindahkan antar-provider, dan dapat dibuktikan hasilnya.

Status dokumen: draft awal, 12 Agustus 2026.

## Masalah

Tim yang memakai coding agent biasanya menghadapi lima masalah:

1. Perilaku berubah saat model atau provider diganti.
2. Tool call memiliki akses terlalu luas atau approval terlalu sering sehingga diabaikan.
3. Konteks panjang membengkak, mahal, dan kehilangan keputusan penting.
4. Pekerjaan multi-step sulit dilanjutkan dan sulit diaudit setelah gagal.
5. Integrasi, aturan, prompt, dan workflow terikat pada satu produk.

ARSY menyediakan control plane lokal untuk masalah-masalah tersebut.

## Pengguna utama

### Individual engineer

Ingin agent yang cepat memahami repo, melakukan perubahan, menjalankan verifikasi, dan tetap meminta izin sebelum aksi berisiko.

### Platform atau AI engineering team

Ingin mengatur provider, policy, budget, observability, tools, dan instruksi lintas banyak repository.

### Security-conscious organization

Ingin agent lokal atau self-hosted dengan allowlist, audit trail, isolasi eksekusi, dan aturan organisasi yang tidak dapat ditimpa oleh prompt proyek.

## Jobs to be done

- Memahami codebase dan menjawab pertanyaan dengan bukti file.
- Mengimplementasikan perubahan kecil sampai besar dengan diff terkontrol.
- Menjalankan test, lint, build, serta mendiagnosis kegagalan.
- Melakukan review kode dan menyajikan temuan yang dapat ditindaklanjuti.
- Menjalankan workflow berulang dari terminal atau CI.
- Mendelegasikan subtask yang independen tanpa kehilangan ownership dan budget.

## Proposisi nilai

**Satu harness, banyak model, satu kebijakan kerja.**

| Pilar | Janji produk |
| --- | --- |
| Control | Pengguna menentukan capability, approval, budget, dan batas eksekusi. |
| Portability | Model, tool, MCP server, dan runner dapat diganti melalui kontrak stabil. |
| Continuity | Sesi tersimpan sebagai event dan dapat dilanjutkan secara deterministik. |
| Evidence | Klaim selesai harus disertai hasil tool, test, atau artefak. |
| Extensibility | Skills dan hooks memperluas workflow tanpa fork mesin inti. |

## Prinsip produk

- **Local-first, remote-capable.** Jalur lokal harus berguna tanpa daemon atau cloud control plane.
- **Human authority is explicit.** Approval memiliki target dan scope, bukan tombol “percaya semua”.
- **Model proposes, runtime disposes.** Model tidak pernah menjadi sumber otorisasi.
- **Progressive complexity.** Single-agent adalah default; orkestrasi hanya dipakai saat manfaatnya nyata.
- **Evidence over confidence.** Confidence model tidak menggantikan test atau hasil command.
- **Clean-room and provider-neutral.** Kompatibilitas pola kerja tidak berarti menyalin kode atau merek produk lain.

## Ruang lingkup

### MVP — local single-agent

- REPL/TUI dan `arsy run` untuk CI atau scripting.
- Streaming response dan tool-call loop dengan batas turn, waktu, token, dan biaya.
- Adapter OpenAI serta Anthropic.
- Tool bawaan: read, search, list, patch, shell, dan git read-only.
- Discovery instruksi `AGENTS.md` dari root menuju current directory.
- Policy engine untuk filesystem, network, process, environment, dan command risk.
- Sandbox workspace, approval sekali, approval rule terbatas, dan deny.
- SQLite event log, checkpoint, resume, branch session, dan export JSONL.
- Context compaction yang menyimpan keputusan, todo, bukti, dan file aktif.
- MCP client untuk STDIO dan Streamable HTTP.
- Metrics lokal: token, biaya estimasi, latency, tool result, dan diff summary.

### V1 — orchestration

- Subagent dengan role, scope file, capability, model, budget, dan deadline.
- DAG task, konkurensi terbatas, cancellation, mailbox, dan result merge.
- Git worktree opsional untuk writer paralel.
- Skills, lifecycle hooks, prompt registry, dan policy packs.
- Remote runner yang memakai protokol eksekusi sama dengan runner lokal.
- Mode plan/review/implement yang dapat dipilih atau dipicu policy.

### Nanti, hanya jika dibutuhkan

- IDE extension.
- Web dashboard organisasi.
- Marketplace publik.
- Semantic index permanen untuk setiap repo.
- Voice, computer use umum, atau fine-tuning.

## Non-goals

- Menjadi editor kode atau pengganti IDE.
- Menjadi model inference server.
- Menjamin kode benar tanpa verifikasi eksternal.
- Menjalankan aksi destruktif tanpa batas hanya karena mode otomatis aktif.
- Mendukung semua provider dan semua sandbox pada rilis pertama.

## Alur pengguna inti

### Perubahan kode interaktif

1. ARSY menemukan root repo, instruksi, status git, dan konfigurasi efektif.
2. Pengguna memberikan tujuan.
3. Agent membaca konteks minimum dan menyusun aksi berikutnya.
4. Tool runtime mengevaluasi policy sebelum setiap side effect.
5. Aksi berisiko meminta approval yang menjelaskan target dan dampaknya.
6. Agent mengubah file, menjalankan verifikasi, dan mereview diff.
7. Sesi berakhir dengan ringkasan perubahan, bukti, risiko tersisa, dan ID sesi.

### Eksekusi non-interaktif

1. Pengguna menjalankan `arsy run` dengan policy profile eksplisit.
2. Aksi yang tidak diizinkan gagal tertutup; proses tidak menunggu prompt tersembunyi.
3. Hasil akhir tersedia sebagai teks manusia dan JSON terstruktur dengan exit code stabil.

### Delegasi

1. Agent utama membuat subtask dengan output contract dan budget.
2. Scheduler menjalankan hanya task independen secara paralel.
3. Writer berbagi workspace secara serial atau memakai worktree terpisah.
4. Agent utama memvalidasi hasil sebelum menggabungkannya ke jawaban akhir.

## Requirement fungsional utama

| ID | Requirement | Kriteria penerimaan ringkas |
| --- | --- | --- |
| FR-01 | Session loop | Satu turn dapat menghasilkan nol atau lebih tool call lalu jawaban final. |
| FR-02 | Resume | Proses yang dihentikan dapat melanjutkan sesi tanpa menggandakan side effect selesai. |
| FR-03 | Provider adapter | Provider dapat diganti tanpa mengubah kontrak tool internal. |
| FR-04 | Policy | Setiap tool call menerima keputusan allow, prompt, sandbox, atau deny. |
| FR-05 | Audit | Semua input, keputusan, tool call, status, dan penggunaan tercatat sebagai event. |
| FR-06 | Context | Compaction mempertahankan tujuan, aturan, keputusan, todo, dan bukti aktif. |
| FR-07 | MCP | Client dapat menemukan dan memanggil tool dengan timeout serta schema validation. |
| FR-08 | Instructions | Instruksi berjenjang memiliki urutan dan sumber yang terlihat pengguna. |
| FR-09 | Verification | Status selesai membedakan klaim terverifikasi dan belum terverifikasi. |
| FR-10 | Cancellation | Pengguna dapat menghentikan turn dan child task tanpa merusak event log. |

## Requirement non-fungsional

- Startup lokal p95 di bawah 500 ms, tidak termasuk autentikasi atau panggilan model.
- Semua event persisten ditulis atomik; crash tidak boleh merusak sesi sebelumnya.
- Secret di-redact sebelum logging dan tidak dipersistenkan dalam payload prompt mentah.
- Tool call memiliki timeout dan output byte limit.
- Mode non-interaktif memiliki exit code dan JSON schema yang versioned.
- Fitur eksperimental berada di belakang flag dan tidak mengubah default aman.

## Model permission

Keputusan policy menggunakan empat hasil:

| Hasil | Arti |
| --- | --- |
| `allow` | Aman dalam capability dan scope saat ini. |
| `sandbox` | Boleh berjalan hanya di boundary yang ditentukan. |
| `prompt` | Membutuhkan persetujuan manusia dengan target spesifik. |
| `deny` | Tidak boleh berjalan pada sesi ini. |

Approval harus menampilkan command atau operasi, working directory, file/network target, alasan, dan apakah izin berlaku sekali atau sebagai rule sempit. Credential access, destructive commands, publish, deploy, dan perubahan di luar workspace tidak boleh ditutupi rule generik.

## Metrik keberhasilan

### Produk

- Task success rate pada suite repo nyata.
- Persentase sesi yang selesai tanpa koreksi manual terhadap diff.
- Resume success rate setelah interruption.
- Approval precision: seberapa banyak prompt yang benar-benar berisiko.
- Waktu median dari prompt ke verifikasi selesai.

### Kualitas agent

- Tool schema validity.
- Regression pass rate.
- Context recall untuk aturan dan keputusan penting.
- Biaya dan token per task berhasil.
- Rasio claim “selesai” yang memiliki bukti verifikasi.

### Keamanan

- Unauthorized side-effect rate harus nol pada test suite adversarial.
- Secret leakage rate harus nol pada log dan prompt capture.
- Persentase tool call yang memiliki policy decision dan audit event harus 100%.

## Roadmap berbasis gate

| Gate | Keluar ketika |
| --- | --- |
| G0 — contracts | Event, provider, tool, policy, dan session schema memiliki test kontrak. |
| G1 — useful loop | Agent lokal dapat memperbaiki fixture repo dan membuktikannya dengan test. |
| G2 — safe loop | Sandbox, approval, redaction, timeout, dan cancellation lulus threat suite. |
| G3 — durable loop | Resume dan compaction lulus crash/replay test. |
| G4 — extensible loop | Satu MCP server dan satu skill bekerja tanpa coupling ke core. |
| G5 — orchestration | Delegasi mengungguli single-agent pada benchmark yang disepakati. |

Tanggal rilis tidak ditentukan sebelum G1 dan G2 lolos. Fitur V1 tidak masuk MVP hanya untuk mengejar parity daftar fitur.

## Risiko dan mitigasi

| Risiko | Mitigasi awal |
| --- | --- |
| Prompt injection dari repo/tool | Pisahkan data dan instruksi, tampilkan sumber, policy tetap di luar prompt. |
| Side effect ganda saat resume | Idempotency key dan status tool call persisten. |
| Context compaction menghapus aturan | Pin policy/instruction digest serta eval recall. |
| Multi-agent mengedit file sama | Single writer default; worktree untuk writer paralel. |
| Provider drift | Contract test, capability negotiation, dan fixture response. |
| Biaya tidak terkendali | Budget per session/agent dan hard stop runtime. |
| Plugin berbahaya | Trust level, capability manifest, signature nanti bila distribusi publik dimulai. |

## Keputusan produk yang masih terbuka

- Nama final dan domain proyek.
- Format konfigurasi publik: TOML saja atau TOML dengan schema JSON.
- Sandbox lintas-platform pertama: macOS/Linux saja atau Windows sejak MVP.
- Distribusi binary dan strategi auto-update.
- Lisensi open source.
- Provider mana yang menjadi reference adapter pertama.

