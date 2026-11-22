//! What a card says: its status, the marks before its name, its detail line.

use super::*;

/// How a status reads and which colour carries it.
pub(in crate::ui) fn status_style(p: &Palette, e: &Entry) -> (&'static str, Color32) {
    match e.download.status() {
        Status::Running if e.download.is_recording() => (tr!("ENREGISTREMENT", "RECORDING"), p.danger),
        Status::Running => (tr!("EN COURS", "ACTIVE"), p.accent),
        Status::Queued if e.retry.is_some() => (tr!("NOUVEL ESSAI", "RETRYING"), p.warning),
        Status::Queued if e.resolving => (tr!("PRÉPARATION", "PREPARING"), p.accent),
        Status::Queued => (tr!("EN FILE", "QUEUED"), p.muted),
        Status::Paused => (tr!("EN PAUSE", "PAUSED"), p.warning),
        Status::Completed => (tr!("TERMINÉ", "DONE"), p.success),
        Status::Failed(_) => (tr!("ÉCHEC", "FAILED"), p.danger),
    }
}

/// Small icons before the name: its queue, its own speed limit, an accepted certificate, the
/// proxy route.
pub(super) fn markers(p: &Palette, queues: &[Queue], e: &Entry) -> Vec<(&'static str, Color32, String)> {
    let d = &e.download;
    let mut marks = Vec::new();
    if d.queue != 0 && queues.iter().any(|q| q.id == d.queue) {
        marks.push((icon::QUEUE, p.accent, trf!("File : {}", "Queue: {}", queue_name(queues, d.queue))));
    }
    if d.speed_limit_kib > 0 {
        let limit = speed(f64::from(d.speed_limit_kib) * 1024.0);
        marks.push((icon::GAUGE, p.warning, trf!("Limité à {limit}", "Limited to {limit}", limit = limit)));
    }
    if d.insecure {
        marks.push((
            icon::SHIELD_WARNING,
            p.danger,
            tr!("Certificat non valide accepté pour ce téléchargement", "Invalid certificate accepted for this download").to_owned(),
        ));
    }
    if e.via_proxy {
        marks.push((icon::SHUFFLE, p.muted, tr!("Passe par le proxy (connexion directe trop lente)", "Goes through the proxy (direct connection too slow)").to_owned()));
    }
    marks
}

pub(in crate::ui) fn detail_line(e: &Entry, status: &Status, recording: bool, done: u64, total: u64, conns: usize, p: &Palette) -> (String, Color32) {
    let size = if total > 0 { bytes(total) } else { tr!("taille inconnue", "unknown size").into() };
    let eta = e.eta().map(|s| trf!(" · reste {}", " · {} left", duration(s))).unwrap_or_default();
    let (got, rate) = (bytes(done), speed(e.speed));
    if let (Status::Queued, Some(retry)) = (status, &e.retry) {
        let left = retry.at.saturating_duration_since(std::time::Instant::now()).as_secs();
        let reason = failure::text(&retry.reason);
        let when = if left == 0 { tr!("maintenant", "now").to_owned() } else { trf!("dans {}", "in {}", duration(left)) };
        return (trf!("{reason} · nouvel essai {when}", "{reason} · retrying {when}", reason = reason, when = when), p.warning);
    }
    let text = match status {
        Status::Running if recording => {
            let estimate = if total > 0 { format!(" / ~{}", bytes(total)) } else { String::new() };
            trf!("{got}{estimate} · {rate}{eta} · dans le navigateur", "{got}{estimate} · {rate}{eta} · in the browser", estimate = estimate, eta = eta, got = got, rate = rate)
        }
        Status::Running => {
            let conns = crate::i18n::count(conns as u64, ("connexion", "connexions"), ("connection", "connections"));
            format!("{got} / {size} · {rate}{eta} · {conns}")
        }
        Status::Completed => {
            let place = e.download.target.parent().map_or(String::new(), |d| d.display().to_string());
            let size = bytes(total.max(done));
            match &e.verify {
                Verify::Ok => return (trf!("{size} · ✓ empreinte vérifiée · {place}", "{size} · ✓ checksum verified · {place}", place = place, size = size), p.success),
                Verify::Mismatch(_) => {
                    return (trf!("✗ empreinte différente : fichier corrompu ou pas le bon · {place}", "✗ checksum mismatch: corrupted or not the expected file · {place}", place = place), p.danger);
                }
                Verify::Running => trf!("{size} · vérification de l'empreinte…", "{size} · verifying the checksum…", size = size),
                Verify::Failed(reason) => return (trf!("{size} · empreinte non vérifiée : {reason}", "{size} · checksum not verified: {reason}", reason = reason, size = size), p.warning),
                Verify::None => format!("{size} · {place}"),
            }
        }
        Status::Paused => format!("{got} / {size}"),
        Status::Queued if e.resolving => tr!("analyse du lien auprès du serveur…", "asking the server about the link…").to_owned(),
        Status::Queued => trf!("{got} / {size} · démarre dès qu'une place se libère", "{got} / {size} · starts as soon as a place is free", got = got, size = size),
        Status::Failed(why) if recording => {
            let reason = failure::text(why);
            return (trf!("{reason} — relancez l'enregistrement depuis la page", "{reason} — start the recording again from the page", reason = reason), p.danger);
        }
        Status::Failed(why) => {
            let reason = failure::text(why);
            return (trf!("{reason} — ▶ pour réessayer", "{reason} — ▶ to retry", reason = reason), p.danger);
        }
    };
    (text, p.muted)
}
