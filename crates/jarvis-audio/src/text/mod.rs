//! Utilidades de texto compartidas por las heurísticas de voz (HU-59).
//!
//! Hoy solo expone [`normalize`], la fuente única de comparación léxica que
//! usan la clasificación de carga, los títulos de tarea y la detección de
//! tareas de desarrollo u operacionales.
//!
//! [`normalize`]: crate::text::normalize::normalize

pub mod normalize;
