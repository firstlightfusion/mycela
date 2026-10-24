/// Lightweight Modbus TCP demo simulator.
///
/// Implements the Modbus TCP Application Data Unit (ADU) framing from scratch
/// using `tokio::net::TcpListener` so no external `server` Cargo feature is
/// required.  The in-memory register bank is updated by a background sine-wave
/// task to simulate live process values.
///
/// # Demo register map (matches the Modbus screen in `examples/demo_app.json`)
///
/// Holding registers:
///   1000  — temperature (physical = raw * 0.01, range 20–30 °C)
///   1001  — pressure    (physical = raw * 0.1,  range 900–1100 hPa)
///   1002  — level setpoint (writable, initial = 500 → 50.0 with scale 0.1)
///   1003  — counter     (increments each tick)
///   1004  — pump mode   (writable enum index, initial = 0)
///   3000+ — unset, read as 0 (alarm-state references add a fixed offset)
///
/// Coils:
///   0  — heartbeat (toggles every ~2 s)
///   1  — pump run command (writable, initial = false)
///   2  — valve B status (toggles every ~3 s, out of phase with coil 0)
///
/// Input registers (read-only):
///   2000  — tank level (physical = raw * 0.01, sweeps 0.5–99.5 %)
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Mutex;

type RegBank = Arc<Mutex<HashMap<u16, u16>>>;
type CoilBank = Arc<Mutex<HashMap<u16, bool>>>;

/// Start the Modbus TCP simulator on the given port.
///
/// Returns `(sim_handle, listener_handle)` — both must be aborted to fully stop.
pub fn start_modbus_simulator(port: u16) -> (tokio::task::JoinHandle<()>, tokio::task::JoinHandle<()>) {
    // ── Initial register values ──────────────────────────────────────────────
    let mut init_regs: HashMap<u16, u16> = HashMap::new();
    init_regs.insert(1000, 2500);  // 25.00 °C  (scale 0.01)
    init_regs.insert(1001, 10000); // 1000.0 hPa (scale 0.1)
    init_regs.insert(1002, 500);   // setpoint 50.0 (scale 0.1)
    init_regs.insert(1003, 0);     // counter
    init_regs.insert(2000, 5000);  // sensor 50.00 (scale 0.01)

    let registers: RegBank = Arc::new(Mutex::new(init_regs));
    let coils: CoilBank    = Arc::new(Mutex::new(HashMap::new()));

    // ── Background simulator task ────────────────────────────────────────────
    let regs_sim  = registers.clone();
    let coils_sim = coils.clone();
    let sim_handle = tokio::spawn(async move {
        let mut phase        = 0.0_f64;
        let mut counter      = 0u16;
        let mut toggle_count = 0u8;
        let mut interval = tokio::time::interval(Duration::from_millis(500));
        loop {
            interval.tick().await;
            phase        += 0.15;
            counter       = counter.wrapping_add(1);
            toggle_count  = toggle_count.wrapping_add(1);

            let temp     = 25.0  + 5.0   * phase.sin();
            let pressure = 1000.0 + 100.0 * (phase * 0.7).sin();
            let sensor   = 50.0  + 49.5  * (phase * 1.3).cos();

            let mut r = regs_sim.lock().await;
            r.insert(1000, (temp     * 100.0_f64) as u16);
            r.insert(1001, (pressure *  10.0_f64) as u16);
            r.insert(1003, counter);
            r.insert(2000, (sensor   * 100.0_f64) as u16);
            drop(r);

            // Toggle status coil every ~2 s (4 half-second ticks)
            if toggle_count % 4 == 0 {
                let mut c = coils_sim.lock().await;
                let current = c.get(&0).copied().unwrap_or(false);
                c.insert(0, !current);
                // Valve B toggles every ~3 s (6 half-second ticks), out of phase
                if toggle_count % 6 == 0 {
                    let current_b = c.get(&2).copied().unwrap_or(false);
                    c.insert(2, !current_b);
                }
            }
        }
    });

    // ── TCP listener task ────────────────────────────────────────────────────
    let listener_handle = tokio::spawn(async move {
        let addr = format!("127.0.0.1:{}", port);
        let listener = match TcpListener::bind(&addr).await {
            Ok(l) => {
                tracing::info!("Modbus simulator listening on {}", addr);
                l
            }
            Err(e) => {
                tracing::error!("Failed to start Modbus simulator on {}: {}", addr, e);
                return;
            }
        };

        loop {
            match listener.accept().await {
                Ok((socket, peer)) => {
                    tracing::debug!("Modbus simulator: connection from {}", peer);
                    let r = registers.clone();
                    let c = coils.clone();
                    tokio::spawn(handle_connection(socket, r, c));
                }
                Err(e) => {
                    tracing::error!("Modbus simulator accept error: {}", e);
                }
            }
        }
    });

    (sim_handle, listener_handle)
}

async fn handle_connection(
    mut socket: tokio::net::TcpStream,
    registers: RegBank,
    coils: CoilBank,
) {
    let mut buf = vec![0u8; 512];
    loop {
        let n = match socket.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };

        if n < 8 {
            continue;
        }

        let transaction_id = u16::from_be_bytes([buf[0], buf[1]]);
        let unit_id = buf[6];
        let fc      = buf[7];

        let pdu = match build_response_pdu(fc, &buf[8..n], &registers, &coils).await {
            Some(p) => p,
            None    => vec![fc | 0x80, 0x01], // unknown FC → exception
        };

        let payload_len = (1 + pdu.len()) as u16;
        let mut resp = Vec::with_capacity(6 + 1 + pdu.len());
        resp.extend_from_slice(&transaction_id.to_be_bytes());
        resp.extend_from_slice(&0u16.to_be_bytes());
        resp.extend_from_slice(&payload_len.to_be_bytes());
        resp.push(unit_id);
        resp.extend_from_slice(&pdu);

        if socket.write_all(&resp).await.is_err() {
            break;
        }
    }
    tracing::debug!("Modbus simulator: connection closed");
}

async fn build_response_pdu(
    fc: u8,
    data: &[u8],
    registers: &RegBank,
    coils: &CoilBank,
) -> Option<Vec<u8>> {
    if data.len() < 4 { return None; }
    let start = u16::from_be_bytes([data[0], data[1]]);
    let count = u16::from_be_bytes([data[2], data[3]]);

    match fc {
        0x01 | 0x02 => {
            let c          = coils.lock().await;
            let byte_count = (count as usize + 7) / 8;
            let mut bits   = vec![0u8; byte_count];
            for i in 0..count as usize {
                if c.get(&(start + i as u16)).copied().unwrap_or(false) {
                    bits[i / 8] |= 1 << (i % 8);
                }
            }
            let mut resp = vec![fc, byte_count as u8];
            resp.extend_from_slice(&bits);
            Some(resp)
        }
        0x03 | 0x04 => {
            let r          = registers.lock().await;
            let byte_count = count as usize * 2;
            let mut values = Vec::with_capacity(byte_count);
            for i in 0..count {
                let val = r.get(&(start + i)).copied().unwrap_or(0);
                values.extend_from_slice(&val.to_be_bytes());
            }
            let mut resp = vec![fc, byte_count as u8];
            resp.extend_from_slice(&values);
            Some(resp)
        }
        0x05 => {
            let coil_val = u16::from_be_bytes([data[2], data[3]]);
            coils.lock().await.insert(start, coil_val == 0xFF00);
            Some(vec![0x05, data[0], data[1], data[2], data[3]])
        }
        0x06 => {
            let val = u16::from_be_bytes([data[2], data[3]]);
            registers.lock().await.insert(start, val);
            Some(vec![0x06, data[0], data[1], data[2], data[3]])
        }
        0x0F => {
            if data.len() < 5 { return None; }
            let byte_count = data[4] as usize;
            if data.len() < 5 + byte_count { return None; }
            let mut c = coils.lock().await;
            for i in 0..count as usize {
                let bit = (data[5 + i / 8] >> (i % 8)) & 1;
                c.insert(start + i as u16, bit != 0);
            }
            Some(vec![0x0F, data[0], data[1], data[2], data[3]])
        }
        0x10 => {
            if data.len() < 5 { return None; }
            let byte_count = data[4] as usize;
            if data.len() < 5 + byte_count { return None; }
            let mut r = registers.lock().await;
            for i in 0..count as usize {
                let off = 5 + i * 2;
                if off + 1 < data.len() {
                    let val = u16::from_be_bytes([data[off], data[off + 1]]);
                    r.insert(start + i as u16, val);
                }
            }
            Some(vec![0x10, data[0], data[1], data[2], data[3]])
        }
        _ => None,
    }
}
