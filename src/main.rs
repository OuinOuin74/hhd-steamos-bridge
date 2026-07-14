/*
 * hhd-steamos-bridge v2
 *
 * Remote D-Bus pour steamos-manager (mécanisme remotes.d) qui relaie
 * TdpLimit1 et GpuPerformanceLevel1 vers Handheld Daemon via la
 * commande `hhd.steamos`.
 *
 * v2 : chaque interface est exposée sur SON PROPRE nom de bus, pris
 * en décalé (GPU seulement après confirmation de l'enregistrement du
 * TDP côté démon). Contourne une course dans steamos-manager 26.3.0 :
 * deux interfaces remote sur un même nom de bus déclenchent deux
 * load_tasks simultanés sur le même NameOwnerChanged, qui
 * s'interbloquent (gel du démon, vérifié par trace zbus).
 *
 * Basé sur l'exemple basic_remote.rs de Valve (MIT).
 * Mapping hhd.steamos identique au fork Bazzite de steamos-manager.
 *
 * Codes de retour de hhd.steamos (cf. src/hhd/http/steamos.py) :
 *   0 = OK
 *   1 = désactivé -> fallback steamos-manager
 *   2 = conflit avec une autre application
 *   3 = échec du set, à ignorer (transitoire)
 *   5 = (gpu) réessayer, transitoire
 *
 * SPDX-License-Identifier: MIT
 */
use anyhow::{anyhow, bail, Result};
use std::time::Duration;
use tokio::process::Command;
use tokio::time::sleep;
use zbus::connection::Builder;
use zbus::interface;

const HHD: &str = "hhd.steamos";
const TDP_BUS_NAME: &str = "com.steampowered.HhdBridge.Tdp";
const GPU_BUS_NAME: &str = "com.steampowered.HhdBridge.Gpu";
const OBJECT_PATH: &str = "/com/steampowered/HhdBridge";

/// Exécute `hhd.steamos <args...>` et retourne (code de sortie, stdout).
async fn hhd(args: &[&str]) -> Result<(i32, String)> {
    let out = Command::new(HHD)
        .args(args)
        .output()
        .await
        .map_err(|e| anyhow!("impossible d'exécuter {HHD}: {e}"))?;
    Ok((
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
    ))
}

fn parse_nums(s: &str) -> Vec<u32> {
    s.split_whitespace().filter_map(|v| v.parse().ok()).collect()
}

/// `hhd.steamos steamos-tdp get` -> (min, max, default)
async fn tdp_limits() -> Result<(u32, u32, u32)> {
    let (code, out) = hhd(&["steamos-tdp", "get"]).await?;
    if code != 0 {
        bail!("steamos-tdp get: code {code}");
    }
    let n = parse_nums(&out);
    if n.len() < 3 {
        bail!("sortie inattendue de steamos-tdp get: {out:?}");
    }
    Ok((n[0], n[1], n[2]))
}

/// `hhd.steamos steamos-gpu get` -> (min, max) en MHz
async fn gpu_limits() -> Result<(u32, u32)> {
    let (code, out) = hhd(&["steamos-gpu", "get"]).await?;
    if code != 0 {
        bail!("steamos-gpu get: code {code}");
    }
    let n = parse_nums(&out);
    if n.len() < 2 {
        bail!("sortie inattendue de steamos-gpu get: {out:?}");
    }
    Ok((n[0], n[1]))
}

fn zerr(msg: String) -> zbus::fdo::Error {
    zbus::fdo::Error::Failed(msg)
}

/// Attend que le démon utilisateur ait fini d'enregistrer TdpLimit1
/// (visible dans son introspection sur le bus session) avant que le
/// second nom de bus soit pris — sérialise les deux enregistrements.
async fn wait_tdp_registered() {
    let session = match zbus::Connection::session().await {
        Ok(c) => c,
        Err(e) => {
            eprintln!("bus session inaccessible ({e}), délai fixe de 3 s");
            sleep(Duration::from_secs(3)).await;
            return;
        }
    };
    for _ in 0..40 {
        if let Ok(reply) = session
            .call_method(
                Some("com.steampowered.SteamOSManager1"),
                "/com/steampowered/SteamOSManager1",
                Some("org.freedesktop.DBus.Introspectable"),
                "Introspect",
                &(),
            )
            .await
        {
            if let Ok(xml) = reply.body().deserialize::<String>() {
                if xml.contains("com.steampowered.SteamOSManager1.TdpLimit1") {
                    println!("TdpLimit1 confirmé côté session, on expose le GPU");
                    return;
                }
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    eprintln!("TdpLimit1 non confirmé après 20 s, on tente le GPU quand même");
    sleep(Duration::from_secs(2)).await;
}

// ---------------------------------------------------------------------------
// com.steampowered.SteamOSManager1.TdpLimit1
// ---------------------------------------------------------------------------

struct TdpLimit1 {
    min: u32,
    max: u32,
    /// hhd.steamos n'expose pas la valeur courante : on met en cache la
    /// dernière valeur écrite, initialisée au TDP par défaut de HHD.
    current: u32,
}

#[interface(name = "com.steampowered.SteamOSManager1.TdpLimit1")]
impl TdpLimit1 {
    #[zbus(property)]
    async fn tdp_limit(&self) -> u32 {
        self.current
    }

    #[zbus(property)]
    async fn set_tdp_limit(&mut self, limit: u32) -> zbus::fdo::Result<()> {
        let (code, _) = hhd(&["steamos-tdp", &limit.to_string()])
            .await
            .map_err(|e| zerr(e.to_string()))?;
        match code {
            0 => {
                println!("TDP -> {limit} W");
                self.current = limit;
                Ok(())
            }
            // 3 = échec transitoire, le contrat dit de l'ignorer
            3 => Ok(()),
            2 => Err(zerr("conflit TDP avec une autre application".into())),
            c => Err(zerr(format!("steamos-tdp {limit}: code {c}"))),
        }
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn tdp_limit_min(&self) -> u32 {
        self.min
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn tdp_limit_max(&self) -> u32 {
        self.max
    }
}

// ---------------------------------------------------------------------------
// com.steampowered.SteamOSManager1.GpuPerformanceLevel1
// ---------------------------------------------------------------------------

struct GpuPerformanceLevel1 {
    min: u32,
    max: u32,
    /// Même principe : cache de la dernière fréquence demandée.
    clock: u32,
}

#[interface(name = "com.steampowered.SteamOSManager1.GpuPerformanceLevel1")]
impl GpuPerformanceLevel1 {
    #[zbus(property(emits_changed_signal = "const"))]
    async fn available_gpu_performance_levels(&self) -> Vec<String> {
        vec!["auto".into(), "manual".into()]
    }

    // Toujours répondre "manual" (même astuce que Bazzite) : Steam
    // redéclenche ainsi explicitement "auto" au démarrage, ce qui
    // remet HHD en mode automatique via `steamos-gpu clear`.
    #[zbus(property)]
    async fn gpu_performance_level(&self) -> String {
        "manual".into()
    }

    #[zbus(property)]
    async fn set_gpu_performance_level(&mut self, level: String) -> zbus::fdo::Result<()> {
        if level == "manual" {
            // Le passage en manuel effectif se fait au premier set de
            // ManualGpuClock ; rien à faire ici.
            return Ok(());
        }
        // "auto" (ou tout autre niveau) -> on rend la main à HHD
        let (code, _) = hhd(&["steamos-gpu", "clear"])
            .await
            .map_err(|e| zerr(e.to_string()))?;
        match code {
            0 => {
                println!("GPU -> auto (clear)");
                Ok(())
            }
            3 | 5 => Ok(()),
            2 => Err(zerr("conflit GPU avec une autre application".into())),
            c => Err(zerr(format!("steamos-gpu clear: code {c}"))),
        }
    }

    #[zbus(property)]
    async fn manual_gpu_clock(&self) -> u32 {
        self.clock
    }

    #[zbus(property)]
    async fn set_manual_gpu_clock(&mut self, clock: u32) -> zbus::fdo::Result<()> {
        let (code, _) = hhd(&["steamos-gpu", &clock.to_string()])
            .await
            .map_err(|e| zerr(e.to_string()))?;
        match code {
            0 => {
                println!("GPU -> {clock} MHz");
                self.clock = clock;
                Ok(())
            }
            3 | 5 => Ok(()),
            2 => Err(zerr("conflit GPU avec une autre application".into())),
            c => Err(zerr(format!("steamos-gpu {clock}: code {c}"))),
        }
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn manual_gpu_clock_min(&self) -> u32 {
        self.min
    }

    #[zbus(property(emits_changed_signal = "const"))]
    async fn manual_gpu_clock_max(&self) -> u32 {
        self.max
    }
}

// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() -> Result<()> {
    // Attendre que HHD soit prêt et que le contrôle TDP soit activé.
    // On ne prend les noms de bus qu'à ce moment-là : steamos-manager
    // enregistre chaque remote dès qu'il voit son nom apparaître
    // (NameOwnerChanged + ping), l'ordre de démarrage est donc sans
    // importance.
    let (tdp_min, tdp_max, tdp_default) = loop {
        match tdp_limits().await {
            Ok(v) => break v,
            Err(e) => {
                eprintln!("en attente de HHD ({e}), nouvel essai dans 2 s");
                sleep(Duration::from_secs(2)).await;
            }
        }
    };
    println!("TDP: min={tdp_min} max={tdp_max} défaut={tdp_default}");

    // TDP d'abord, seul sur son nom de bus.
    let _tdp_conn = Builder::system()?
        .name(TDP_BUS_NAME)?
        .serve_at(
            OBJECT_PATH,
            TdpLimit1 {
                min: tdp_min,
                max: tdp_max,
                current: tdp_default,
            },
        )?
        .build()
        .await?;
    println!("TdpLimit1 exposé sur {TDP_BUS_NAME}");

    // GPU ensuite, sur SON nom, une fois le TDP enregistré côté démon.
    let mut gpu_conn = None;
    match gpu_limits().await {
        Ok((gpu_min, gpu_max)) => {
            println!("GPU: min={gpu_min} MHz max={gpu_max} MHz");
            wait_tdp_registered().await;
            gpu_conn = Some(
                Builder::system()?
                    .name(GPU_BUS_NAME)?
                    .serve_at(
                        OBJECT_PATH,
                        GpuPerformanceLevel1 {
                            min: gpu_min,
                            max: gpu_max,
                            clock: gpu_max,
                        },
                    )?
                    .build()
                    .await?,
            );
            println!("GpuPerformanceLevel1 exposé sur {GPU_BUS_NAME}");
        }
        Err(e) => {
            eprintln!(
                "contrôle GPU indisponible ({e}) : seul le TDP est exposé. \
                 Redémarrer ce service après activation du GPU dans HHD."
            );
        }
    }

    // Attendre SIGTERM (systemd) ou Ctrl-C.
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = sigterm.recv() => {},
        _ = tokio::signal::ctrl_c() => {},
    }

    // Arrêt ordonné : le GPU disparaît d'abord, puis le TDP — les deux
    // désenregistrements côté démon sont ainsi sérialisés aussi (même
    // course potentielle au unload qu'au load).
    if let Some(c) = gpu_conn.take() {
        drop(c);
        sleep(Duration::from_secs(1)).await;
    }
    Ok(())
}
