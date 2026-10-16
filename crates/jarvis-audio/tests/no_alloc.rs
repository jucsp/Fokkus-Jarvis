//! CA-1.3: los callbacks de audio no asignan memoria.
//!
//! Este binario de test instala un `#[global_allocator]` contador sobre
//! `std::alloc::System`. El conteo es por hilo (`thread_local` con
//! inicialización `const`, que no asigna), así que los tests que corren en
//! paralelo no se contaminan entre sí.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use jarvis_audio::dsp::{MonoResampler, Reframer};
use std::sync::Arc;

use jarvis_audio::io::{
    process_input_frame, process_output_frame, Fader, InputPort, OutputPort, PortShared,
};
use jarvis_audio::AudioPlayback;
use ringbuf::traits::{Producer, Split};
use ringbuf::HeapRb;

struct CountingAllocator;

thread_local! {
    static ALLOCATIONS: Cell<usize> = const { Cell::new(0) };
}

fn bump() {
    // `try_with`: durante la destrucción del hilo el TLS ya no existe.
    let _ = ALLOCATIONS.try_with(|c| c.set(c.get() + 1));
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: CountingAllocator = CountingAllocator;

/// Asignaciones hechas por `f` en el hilo actual.
fn allocations_in(f: impl FnOnce()) -> usize {
    let before = ALLOCATIONS.with(Cell::get);
    f();
    ALLOCATIONS.with(Cell::get) - before
}

#[test]
fn counter_detects_allocations() {
    // Control positivo: si el contador no ve esto, el resto no prueba nada.
    let n = allocations_in(|| {
        let v = std::hint::black_box(vec![1u8; 64]);
        drop(v);
    });
    assert!(n >= 1);
}

#[test]
fn input_callback_does_not_allocate() {
    let (mut producer, _consumer) = HeapRb::<f32>::new(1_024).split();
    let mut scratch = vec![0.0f32; 256];
    let stereo: Vec<f32> = (0..2_048)
        .map(|i| ((i % 100) as f32 / 50.0) - 1.0)
        .collect();
    let mut dropped = 0u64;
    let n = allocations_in(|| {
        // Varios callbacks: llena el ring y fuerza pérdidas y tramos.
        for _ in 0..8 {
            dropped += process_input_frame(&stereo, 2, &mut scratch, &mut producer);
        }
    });
    assert_eq!(n, 0, "el callback de entrada asignó {n} veces");
    assert!(dropped > 0, "el ring debía llenarse");
}

#[test]
fn output_callback_does_not_allocate_including_fade_and_drain() {
    let (mut tts_prod, mut tts_cons) = HeapRb::<f32>::new(48_000).split();
    let (mut render_prod, _render_cons) = HeapRb::<f32>::new(4_096).split();
    tts_prod.push_slice(&vec![0.5f32; 20_000]);
    let mut scratch = vec![0.0f32; 512];
    let mut out = vec![0.0f32; 1_024]; // 512 frames estéreo
    let mut fader = Fader::new(Fader::frames_for_rate(48_000), 0);

    let n = allocations_in(|| {
        for cb in 0..16u64 {
            // Callbacks 4..: stop() → generación 1 (rampa + drenado).
            let generation = u64::from(cb >= 4);
            process_output_frame(
                &mut tts_cons,
                &mut scratch,
                &mut fader,
                cb >= 4,
                generation,
                &mut render_prod,
                &mut out,
                2,
            );
        }
    });
    assert_eq!(n, 0, "el callback de salida asignó {n} veces");
    assert!(
        out.iter().all(|s| *s == 0.0),
        "tras la parada debe haber silencio"
    );
}

#[test]
fn reframer_and_resampler_do_not_allocate_per_frame() {
    let mut resampler = MonoResampler::new(48_000, 16_000).expect("48 kHz");
    let mut to_160 = Reframer::new(160);
    let mut to_256 = Reframer::new(256);
    let chunk: Vec<f32> = (0..480).map(|i| (i as f32 * 0.01).sin() * 0.5).collect();
    let mut frames = 0usize;
    // Calentamiento fuera de la medición (el primer tramo inicializa el FFT).
    resampler.process(&chunk, |_| {}).expect("remuestreo");

    let n = allocations_in(|| {
        for _ in 0..100 {
            let _ = resampler.process(&chunk, |out| {
                to_160.push(out, |f160| to_256.push(f160, |_| frames += 1));
            });
        }
    });
    assert_eq!(n, 0, "el re-encuadre/remuestreo asignó {n} veces");
    assert!(frames > 0);
}

#[test]
fn engine_input_port_does_not_allocate_for_f32_or_i16() {
    let shared = Arc::new(PortShared::new(AudioPlayback::new()));
    let (producer, _consumer) = HeapRb::<f32>::new(4_096).split();
    let mut port = InputPort::new(producer, 2, Arc::clone(&shared));
    // Más grande que el tramo interno: obliga a procesar por partes.
    let f32_block: Vec<f32> = (0..10_000).map(|i| (i as f32 * 0.01).sin()).collect();
    let i16_block: Vec<i16> = (0..10_000).map(|i| (i * 7 % 30_000) as i16).collect();
    let n = allocations_in(|| {
        for _ in 0..4 {
            port.on_f32(&f32_block);
            port.on_i16(&i16_block);
            port.on_latency(std::time::Duration::from_millis(3));
        }
    });
    assert_eq!(n, 0, "el puerto de entrada asignó {n} veces");
    assert!(
        shared.metrics.snapshot().input_dropped > 0,
        "el ring debía llenarse"
    );
}

#[test]
fn engine_output_port_does_not_allocate_including_barge_in() {
    let playback = AudioPlayback::new();
    let shared = Arc::new(PortShared::new(playback.clone()));
    shared
        .render_tap
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let (mut tts, tts_cons) = HeapRb::<f32>::new(48_000).split();
    let (render, _render_cons) = HeapRb::<f32>::new(2_048).split();
    let mut port = OutputPort::new(tts_cons, render, 48_000, 2, Arc::clone(&shared));
    tts.push_slice(&vec![0.5f32; 40_000]);
    let mut out_f32 = vec![0.0f32; 6_000];
    let mut out_i16 = vec![0i16; 6_000];

    let n = allocations_in(|| {
        for cb in 0..12 {
            if cb == 2 {
                playback.pause();
            }
            if cb == 3 {
                playback.resume();
            }
            if cb == 4 {
                playback.pause();
                playback.stop();
            }
            port.fill_f32(&mut out_f32);
            port.fill_i16(&mut out_i16);
            port.on_latency(std::time::Duration::from_millis(20));
        }
    });
    assert_eq!(n, 0, "el puerto de salida asignó {n} veces");
    assert!(out_f32.iter().all(|s| *s == 0.0), "silencio tras la parada");
    assert!(out_i16.iter().all(|s| *s == 0));
}
