//! Utilidades de texto compartidas por las heurísticas de voz (HU-59, HU-70).
//!
//! Dos criterios distintos, a propósito:
//!
//! - [`normalize`]: comparación **con pérdida** (minúsculas, sin tildes, sin
//!   puntuación) usada por la clasificación de carga, los títulos de tarea y la
//!   detección de tareas de desarrollo u operacionales.
//! - [`lexicon`]: corrección **sin pérdida** de la transcripción de whisper
//!   ("cubernetis" → "Kubernetes"), porque su salida se habla, se muestra en el
//!   HUD y se manda al LLM.
//!
//! [`normalize`]: crate::text::normalize::normalize
//! [`lexicon`]: crate::text::lexicon

pub mod lexicon;
pub mod normalize;
