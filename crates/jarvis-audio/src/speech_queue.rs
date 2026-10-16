//! Cola de locuciones priorizadas para la entrega vocal de resultados (HU-37).
//!
//! Reemplaza la antigua `deferred: VecDeque<String>` del orquestador por una
//! cola con prioridad (`Critical > Reply > TaskResult > Notice`) y FIFO dentro
//! de cada prioridad. La política de entrega la decide [`DeliveryGate`]: Jarvis
//! solo habla si está en reposo/escucha, no hay otra locución en curso, el
//! último instante de voz del usuario quedó al menos `quiet_gap` atrás y no
//! está silenciado ni en modo juego (salvo prioridad `Critical`).

use std::cmp::Ordering;
use std::collections::BinaryHeap;
use std::time::Instant;

use jarvis_core::JarvisState;

/// Prioridad de una locución. El orden de declaración define la precedencia:
/// variantes posteriores tienen más prioridad.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SpeechPriority {
    /// Avisos informativos (IPC, notificaciones).
    Notice,
    /// Resultado de una tarea pesada de fondo.
    TaskResult,
    /// Respuesta del turno conversacional actual.
    Reply,
    /// Errores de seguridad / confirmaciones que no admiten espera.
    Critical,
}

/// Locución encolada para ser dicha por voz.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    pub text: String,
    pub priority: SpeechPriority,
    /// Tarea de origen, si la locución es un resultado de tarea.
    pub task: Option<u64>,
    pub enqueued_at: Instant,
}

impl Delivery {
    pub fn new(text: impl Into<String>, priority: SpeechPriority) -> Self {
        Self {
            text: text.into(),
            priority,
            task: None,
            enqueued_at: Instant::now(),
        }
    }
}

/// Condiciones que debe cumplir el instante para permitir hablar.
#[derive(Debug, Clone)]
pub struct DeliveryGate {
    pub state: JarvisState,
    pub speaking: bool,
    pub last_voice_activity: Option<Instant>,
    pub quiet_gap: std::time::Duration,
    pub muted: bool,
    pub gaming_mode: bool,
}

impl DeliveryGate {
    /// `true` si la locución puede entregarse ahora.
    pub fn allows(&self, priority: SpeechPriority, now: Instant) -> bool {
        if priority >= SpeechPriority::Critical {
            return true;
        }
        if self.muted || self.gaming_mode || self.speaking {
            return false;
        }
        if !matches!(self.state, JarvisState::Idle | JarvisState::Listening) {
            return false;
        }
        if let Some(last) = self.last_voice_activity {
            if now.saturating_duration_since(last) < self.quiet_gap {
                return false;
            }
        }
        true
    }
}

/// Elemento interno del montículo: prioridad + secuencia de llegada.
struct Queued {
    priority: SpeechPriority,
    seq: u64,
    delivery: Delivery,
}

impl Ord for Queued {
    fn cmp(&self, other: &Self) -> Ordering {
        // Mayor prioridad primero; dentro de la misma prioridad, la secuencia
        // menor (la que llegó antes) es "mayor" para que BinaryHeap la saque antes.
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

impl PartialOrd for Queued {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for Queued {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
    }
}

impl Eq for Queued {}

/// Capacidad máxima de la cola. Al excederse se descartan avisos `Notice`
/// antiguos, nunca resultados de tareas.
pub const SPEECH_QUEUE_CAPACITY: usize = 32;

/// Cola de locuciones priorizadas, con capacidad limitada.
#[derive(Default)]
pub struct SpeechQueue {
    heap: BinaryHeap<Queued>,
    next_seq: u64,
}

impl SpeechQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Encola una locución y devuelve su posición (1 = la siguiente en salir).
    /// `0` significa que fue rechazada (cola llena de elementos no descartables).
    pub fn push(&mut self, delivery: Delivery) -> u32 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);
        if !self.make_room() {
            return 0;
        }
        self.heap.push(Queued {
            priority: delivery.priority,
            seq,
            delivery,
        });
        self.position(seq)
    }

    /// Devuelve la locución de mayor prioridad si el *gate* la permite.
    pub fn pop_ready(&mut self, gate: &DeliveryGate, now: Instant) -> Option<Delivery> {
        let top = self.heap.peek()?;
        if gate.allows(top.priority, now) {
            self.heap.pop().map(|q| q.delivery)
        } else {
            None
        }
    }

    /// Número de locuciones pendientes.
    pub fn pending(&self) -> usize {
        self.heap.len()
    }

    /// Hace hueco para una locución entrante. Devuelve `false` si no hay hueco
    /// (cola llena sin `Notice` descartable).
    fn make_room(&mut self) -> bool {
        if self.heap.len() < SPEECH_QUEUE_CAPACITY {
            return true;
        }
        // Saca el aviso `Notice` más antiguo para hacer sitio. Sin avisos que
        // evacuar (cola llena de resultados), se rechaza la entrante: nunca se
        // descarta un `TaskResult`.
        let mut all = std::mem::take(&mut self.heap).into_vec();
        let oldest_notice = all
            .iter()
            .enumerate()
            .filter(|(_, q)| q.priority == SpeechPriority::Notice)
            .min_by_key(|(_, q)| q.seq)
            .map(|(i, _)| i);
        let room = oldest_notice.map(|idx| all.swap_remove(idx)).is_some();
        self.heap = BinaryHeap::from(all);
        room
    }

    /// Posición (1-based) de la locución recién encolada con `seq`.
    fn position(&self, seq: u64) -> u32 {
        let mut all: Vec<&Queued> = self.heap.iter().collect();
        all.sort_by(|a, b| b.cmp(a));
        all.iter()
            .position(|q| q.seq == seq)
            .map(|i| i as u32 + 1)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(state: JarvisState) -> DeliveryGate {
        DeliveryGate {
            state,
            speaking: false,
            last_voice_activity: None,
            quiet_gap: std::time::Duration::from_millis(1_500),
            muted: false,
            gaming_mode: false,
        }
    }

    #[test]
    fn priority_beats_fifo_but_fifo_holds_within_priority() {
        let mut q = SpeechQueue::new();
        let a = q.push(Delivery::new("notice 1", SpeechPriority::Notice));
        let b = q.push(Delivery::new("task 1", SpeechPriority::TaskResult));
        let c = q.push(Delivery::new("notice 2", SpeechPriority::Notice));
        let d = q.push(Delivery::new("task 2", SpeechPriority::TaskResult));
        // notice 2 queda detrás de task 1 y notice 1; task 2 detrás de task 1.
        assert_eq!((a, b, c, d), (1, 1, 3, 2), "posición devuelta");

        let now = Instant::now();
        let g = gate(JarvisState::Idle);
        assert_eq!(
            q.pop_ready(&g, now).map(|d| d.text),
            Some("task 1".to_string())
        );
        assert_eq!(
            q.pop_ready(&g, now).map(|d| d.text),
            Some("task 2".to_string())
        );
        assert_eq!(
            q.pop_ready(&g, now).map(|d| d.text),
            Some("notice 1".to_string())
        );
        assert_eq!(
            q.pop_ready(&g, now).map(|d| d.text),
            Some("notice 2".to_string())
        );
        assert_eq!(q.pop_ready(&g, now), None);
    }

    #[test]
    fn capacity_evicts_oldest_notice_and_never_task_results() {
        let mut q = SpeechQueue::new();
        // Llena con 32 resultados de tarea.
        for i in 0..32 {
            q.push(Delivery::new(
                format!("task {i}"),
                SpeechPriority::TaskResult,
            ));
        }
        assert_eq!(q.pending(), 32);

        // Un aviso entrante con la cola llena de resultados: se rechaza.
        assert_eq!(q.push(Delivery::new("notice", SpeechPriority::Notice)), 0);
        assert_eq!(q.pending(), 32);

        // Llena la cola con avisos y resultados: el aviso más antiguo cae.
        let mut q = SpeechQueue::new();
        for i in 0..SPEECH_QUEUE_CAPACITY {
            q.push(Delivery::new(format!("notice {i}"), SpeechPriority::Notice));
        }
        assert_eq!(q.pending(), SPEECH_QUEUE_CAPACITY);
        q.push(Delivery::new("important", SpeechPriority::TaskResult));
        assert_eq!(q.pending(), SPEECH_QUEUE_CAPACITY, "se descartó un aviso");
        let now = Instant::now();
        let g = gate(JarvisState::Idle);
        assert_eq!(
            q.pop_ready(&g, now).map(|d| d.text),
            Some("important".to_string()),
            "el resultado salta delante de los avisos"
        );

        // Con la cola llena de resultados, un resultado entrante no se descarta
        // (no hay avisos que evacuar): se rechaza para no perder los existentes.
        let mut q = SpeechQueue::new();
        for i in 0..SPEECH_QUEUE_CAPACITY {
            q.push(Delivery::new(
                format!("task {i}"),
                SpeechPriority::TaskResult,
            ));
        }
        assert_eq!(
            q.push(Delivery::new("overflow", SpeechPriority::TaskResult)),
            0
        );
    }

    #[test]
    fn gate_blocks_unless_idle_or_listening() {
        let mut q = SpeechQueue::new();
        let now = Instant::now();
        q.push(Delivery::new("hola", SpeechPriority::TaskResult));
        for state in [
            JarvisState::Thinking,
            JarvisState::Speaking,
            JarvisState::Muted,
        ] {
            let g = gate(state);
            assert_eq!(q.pop_ready(&g, now), None, "estado {state:?}");
        }
        for state in [JarvisState::Idle, JarvisState::Listening] {
            q.push(Delivery::new("hola", SpeechPriority::TaskResult));
            let g = gate(state);
            assert!(q.pop_ready(&g, now).is_some(), "estado {state:?}");
        }
    }

    #[test]
    fn gate_respects_each_condition_independently() {
        let mut q = SpeechQueue::new();
        q.push(Delivery::new("hola", SpeechPriority::TaskResult));
        let now = Instant::now();

        let mut g = gate(JarvisState::Idle);
        g.speaking = true;
        assert_eq!(q.pop_ready(&g, now), None);

        let mut g = gate(JarvisState::Idle);
        g.muted = true;
        assert_eq!(q.pop_ready(&g, now), None);

        let mut g = gate(JarvisState::Idle);
        g.gaming_mode = true;
        assert_eq!(q.pop_ready(&g, now), None);

        let mut g = gate(JarvisState::Idle);
        g.last_voice_activity = Some(now);
        g.quiet_gap = std::time::Duration::from_millis(1_500);
        assert_eq!(q.pop_ready(&g, now), None, "voz reciente bloquea");

        let mut g = gate(JarvisState::Idle);
        g.last_voice_activity = Some(now - std::time::Duration::from_millis(2_000));
        g.quiet_gap = std::time::Duration::from_millis(1_500);
        assert!(
            q.pop_ready(&g, now).is_some(),
            "silencio suficiente entrega"
        );
    }

    #[test]
    fn critical_bypasses_mute_and_gaming() {
        let mut q = SpeechQueue::new();
        q.push(Delivery::new("alerta", SpeechPriority::Critical));
        let now = Instant::now();
        let mut g = gate(JarvisState::Muted);
        g.muted = true;
        g.gaming_mode = true;
        assert!(q.pop_ready(&g, now).is_some());
    }
}
