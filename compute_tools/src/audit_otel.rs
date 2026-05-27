//! feat-031 · compute_tools 侧 audit log → OTel event tail processor。
//!
//! PostgreSQL pgaudit / `pg_session_jwt.audit_log` 输出落在 compute 的 `log_directory`
//! (`<pgdata>/log/audit*.log`)。本模块把这些 audit log file **逐行 tail** 转成
//! `tracing::info!(target: "openneon::audit", ...)` event(经 `audit_event!` macro),让
//! OtelGuard 自动 export 到用户 OTLP collector —— 等价于把 PostgreSQL audit extension
//! 输出走 OTel(详 [feat-031 §3.2 (b) compute_tools 改动 + §11 OQ5]
//! (<https://github.com/zlxtqbdgdgd/openneon-design/blob/main/features/feat-031-L2-neon-audit-log-otel-export.html>))。
//!
//! 跟 mcp 侧(zlxtqbdgdgd/openneon-mcp#110)统一 `openneon.audit.*` attribute schema ·
//! `event_type=compute_audit_log_record`。
//!
//! **L2a 范围(§11 OQ5)**:仅 basic case —— periodic poll 各 `audit*.log` 文件 · 记 per-file
//! byte offset · 只读 offset 之后的新行 emit。**L3+ 加 robust rotate**(inotify / imfile-state
//! 对齐 / 跨 rotate 续读)。文件被 pgaudit GC 删除后 offset 自动重置(metadata 读不到即从 0 起)。
//!
//! USR(feat-008-011 L2b):`openneon.usr.*` namespace 留 hook · L2a 仅填 endpoint_id /
//! project_id(已由 cplane 下发)· tenant_id / timeline_id / shard_id 待 L2b 自动 propagate。

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use tracing::{instrument, warn};

use utils::audit_event;
use utils::logging::audit_event_type;

/// poll 间隔。pgaudit `log_rotation_age` 默认按分钟级 · 5s poll 足够低延迟 + 低开销。
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// 单行 audit log 的最大长度(防御:某行异常巨大不把内存撑爆 · 超长截断)。
const MAX_LINE_BYTES: usize = 64 * 1024;

/// 一个 audit log file 的读取进度(byte offset)。文件 inode 变了(rotate / 重建)→ size
/// 回退到 < offset → 视为新文件从 0 重读。
#[derive(Default)]
struct TailState {
    /// file path → 已读到的 byte offset
    offsets: HashMap<PathBuf, u64>,
}

impl TailState {
    /// 扫一遍 log_directory 下所有 `audit*.log` · 读各文件 offset 之后的新行 · 逐行 emit。
    fn poll_and_emit(&mut self, log_directory: &Path, endpoint_id: &str, project_id: &str) {
        let entries = match std::fs::read_dir(log_directory) {
            Ok(e) => e,
            Err(err) => {
                // 目录还没建 / 权限错 · 下个 tick 再试(不 emit · 不阻塞)
                warn!("audit_otel: read_dir {log_directory:?} failed: {err}");
                return;
            }
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if !is_audit_log_file(&path) {
                continue;
            }
            if let Err(err) = self.tail_one(&path, endpoint_id, project_id) {
                warn!("audit_otel: tail {path:?} failed: {err}");
            }
        }
    }

    fn tail_one(&mut self, path: &Path, endpoint_id: &str, project_id: &str) -> Result<()> {
        let metadata = std::fs::metadata(path)?;
        let size = metadata.len();

        let offset = self.offsets.entry(path.to_path_buf()).or_insert(0);
        // 文件被 truncate / rotate 重建(变小)→ 从头重读(basic rotate handling · L2a)
        if size < *offset {
            *offset = 0;
        }
        if size == *offset {
            return Ok(()); // 无新内容
        }

        let mut file = std::fs::File::open(path)?;
        file.seek(SeekFrom::Start(*offset))?;
        let mut reader = BufReader::new(file);

        let mut consumed = *offset;
        loop {
            let mut line = String::new();
            let n = read_line_capped(&mut reader, &mut line)?;
            if n == 0 {
                break; // EOF
            }
            consumed += n as u64;
            let trimmed = line.trim_end_matches(['\n', '\r']);
            if trimmed.is_empty() {
                continue;
            }
            emit_audit_record(trimmed, endpoint_id, project_id);
        }
        *offset = consumed;
        Ok(())
    }
}

/// 是否是要 tail 的 audit log 文件(`audit*.log` · 跟 pgaudit GC 同 glob)。
fn is_audit_log_file(path: &Path) -> bool {
    if !path.is_file() {
        return false;
    }
    match path.file_name().and_then(|n| n.to_str()) {
        Some(name) => name.starts_with("audit") && name.ends_with(".log"),
        None => false,
    }
}

/// 读一行 · 长度封顶 MAX_LINE_BYTES(防御超长行)。返回消耗的字节数(含换行符)。
fn read_line_capped<R: BufRead>(reader: &mut R, out: &mut String) -> Result<usize> {
    let mut buf = Vec::new();
    let n = reader.take(MAX_LINE_BYTES as u64).read_until(b'\n', &mut buf)?;
    // lossy: audit log 理论上 UTF-8 · 异常字节不丢整行
    out.push_str(&String::from_utf8_lossy(&buf));
    Ok(n)
}

/// emit 一条 `compute_audit_log_record` audit event。
///
/// **PII redact(§6)**:audit log 行**整行不当 SQL 全文落** —— pgaudit 行本身含 statement
/// 文本,这里只放 `db.statement.sha256`(行内容的 sha256)+ 行长度,不放原文,跟 mcp 侧
/// `db.statement.sha256` 一致(攻击痕迹用行 hash join · 全文留在 compute 本地受控 audit 文件)。
fn emit_audit_record(line: &str, endpoint_id: &str, project_id: &str) {
    let sha = sha256_hex(line.as_bytes());
    audit_event!(
        event_type = audit_event_type::COMPUTE_AUDIT_LOG_RECORD,
        // 必填四件套:op_class / principal 补齐(详 docs/audit-otel-schema.md「Required attributes」)。
        // compute_audit_log_record 是 pgaudit 原始行 tail 转发,tail 时无法重建 OpClass
        // → 给 UNCLASSIFIED(L3+ 解析 pgaudit 行 statement 类型再细分)。
        op_class = "UNCLASSIFIED",
        // 这条记录由 compute_ctl(service.name=neon-compute-ctl)代 PostgreSQL audit
        // extension 出口,非人也非 agent → system:<component> 形式(design §3.2 a `system:odd-mrc` 同形)。
        principal = "system:compute-ctl",
        outcome = "allow",
        "db.system" = "postgresql",
        "db.statement.sha256" = %sha,
        "openneon.audit.record_bytes" = line.len(),
        // USR hook(L2a 仅 endpoint_id / project_id · 其余 L2b propagate)。
        // project_id 跟 tenant/timeline/shard 同属 USR 身份字段 → openneon.usr.* namespace。
        "openneon.usr.endpoint_id" = %endpoint_id,
        "openneon.usr.project_id" = %project_id,
    );
}

/// SHA-256 hex(复用 `ring` · compute_tools 已有直接 dep · 不引入新 crate · §9 复用纪律)。
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, bytes);
    let mut s = String::with_capacity(digest.as_ref().len() * 2);
    for b in digest.as_ref() {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[instrument(skip_all, fields(log_directory = %log_directory))]
async fn audit_otel_tail_loop(log_directory: String, endpoint_id: String, project_id: String) {
    let dir = PathBuf::from(&log_directory);
    let mut state = TailState::default();
    loop {
        state.poll_and_emit(&dir, &endpoint_id, &project_id);
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// 启动后台 task,把 compute audit log file 逐行 tail → OTel `compute_audit_log_record` event。
///
/// 跟 `launch_pgaudit_gc` 同 log_directory 启动(compute.rs)· 二者独立(GC 删旧文件 · 本 task
/// 读新行)· offset 续读不受 GC 影响(文件没了 offset 失效即从 0 起)。
pub fn launch_audit_otel_tail(log_directory: String, endpoint_id: String, project_id: String) {
    tokio::spawn(async move {
        audit_otel_tail_loop(log_directory, endpoint_id, project_id).await;
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_hex_known_vector() {
        // sha256("") = e3b0c442...
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn is_audit_log_file_matches_glob() {
        // 仅按文件名前后缀判定(不存在的路径 is_file()=false · 这里只验命名规则)
        assert!(!is_audit_log_file(Path::new("/nonexistent/audit.log")));
        assert!(!is_audit_log_file(Path::new("/nonexistent/postgres.log")));
    }
}
