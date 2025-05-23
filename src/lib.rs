#![deny(clippy::all)]
//#![deny(clippy::pedantic)]
pub mod audio;
mod backends;
pub mod error;
mod jni;
pub mod metadata;
use crate::audio::*;
use crate::backends::*;
use crate::error::OutputError;
use crate::metadata::*;
use anyhow::anyhow;
use rodio::{buffer::SamplesBuffer, OutputStream, Sink};
use std::any::Any;
use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::sync::{mpsc, OnceLock};
use std::thread;
thread_local! {
  static BACKENDS: RefCell<HashMap<String, Box<dyn Backend>>> = RefCell::new(HashMap::new());
  static OUTPUT_STREAM: OnceCell<Option<OutputStream>> = const {OnceCell::new() };
}
static SINK: OnceLock<Sink> = OnceLock::new();
type OperationOk = Box<dyn Any + Send + Sync>;
type OperationResult = Result<OperationOk, OutputError>;
type Operation = Box<dyn FnOnce() -> OperationResult + Send + Sync>;
static OPERATION_TX: OnceLock<mpsc::Sender<(Operation, mpsc::Sender<OperationResult>)>> =
  OnceLock::new();
pub fn initialize() -> Result<(), OutputError> {
  let (operation_tx, operation_rx) = mpsc::channel();
  OPERATION_TX
    .set(operation_tx)
    .map_err(|_| OutputError::into_initialize_failed(anyhow!("Failed to set OPERATION_TX")))?;
  let (result_tx, result_rx) = mpsc::channel();
  thread::spawn(move || {
    let closure = || {
      let (output_stream, output_stream_handle) =
        OutputStream::try_default().map_err(OutputError::into_initialize_failed)?;
      let sink =
        Sink::try_new(&output_stream_handle).map_err(OutputError::into_initialize_failed)?;
      let _result = OUTPUT_STREAM.with(|cell| cell.set(Some(output_stream)));
      let _result = SINK.set(sink);
      let mut backends: Vec<Result<Box<dyn Backend>, OutputError>> = Vec::new();
      backends.push(espeak_ng::EspeakNg::new().map(|value| Box::new(value) as Box<dyn Backend>));
      #[cfg(windows)]
      {
        backends.push(sapi::Sapi::new().map(|value| Box::new(value) as Box<dyn Backend>));
        backends.push(one_core::OneCore::new().map(|value| Box::new(value) as Box<dyn Backend>));
        backends.push(jaws::Jaws::new().map(|value| Box::new(value) as Box<dyn Backend>));
        backends.push(nvda::Nvda::new().map(|value| Box::new(value) as Box<dyn Backend>));
      }
      #[cfg(target_os = "linux")]
      {
        backends.push(
          speech_dispatcher::SpeechDispatcher::new()
            .map(|value| Box::new(value) as Box<dyn Backend>),
        );
      }
      #[cfg(target_os = "macos")]
      {
        backends.push(
          av_speech_synthesizer::AvSpeechSynthesizer::new()
            .map(|value| Box::new(value) as Box<dyn Backend>),
        );
      }
      BACKENDS.set(
        backends
          .into_iter()
          .flatten()
          .map(|backend| (backend.name(), backend))
          .collect(),
      );
      Ok(())
    };
    result_tx.send(closure()).unwrap();
    for (operation, sender) in operation_rx {
      sender.send(operation()).unwrap();
    }
  });
  result_rx
    .recv()
    .map_err(OutputError::into_initialize_failed)?
}
pub fn perform_operation(closure: Operation) -> OperationResult {
  let (result_tx, result_rx) = mpsc::channel();
  OPERATION_TX
    .get()
    .ok_or(OutputError::into_unknown(anyhow!(
      "OPERATION_TX contains no channel"
    )))?
    .send((closure, result_tx))
    .map_err(OutputError::into_unknown)?;
  result_rx.recv().map_err(OutputError::into_unknown)?
}
fn internal_list_voices() -> Result<Vec<Voice>, OutputError> {
  BACKENDS.with_borrow(|backends| {
    let voices = backends
      .values()
      .flat_map(|backend| backend.list_voices())
      .flatten()
      .collect::<Vec<Voice>>();
    Ok(voices)
  })
}
pub fn list_voices() -> Result<Vec<Voice>, OutputError> {
  let closure = || Ok(Box::new(internal_list_voices()?) as OperationOk);
  let result = perform_operation(Box::new(closure))?
    .downcast()
    .map_err(|_| OutputError::into_unknown(anyhow!("Failed to downcast received return value")))?;
  Ok(*result)
}
pub fn list_braille_backends() -> Result<Vec<BrailleBackendMetadata>, OutputError> {
  let closure = || {
    BACKENDS.with_borrow(|backends| {
      let backends = backends
        .values()
        .flat_map(|backend| backend.braille_metadata())
        .collect::<Vec<BrailleBackendMetadata>>();
      Ok(Box::new(backends) as OperationOk)
    })
  };
  let result = perform_operation(Box::new(closure))?
    .downcast()
    .map_err(|_| OutputError::into_unknown(anyhow!("Failed to downcast received return value")))?;
  Ok(*result)
}
fn filter_synthesizers(
  synthesizer: Option<&str>,
  voice: Option<&str>,
  language: Option<&str>,
  audio_data_needed: bool,
) -> Result<String, OutputError> {
  let synthesizer = match (synthesizer, voice, language) {
    (Some(synthesizer), _, _) => synthesizer.to_owned(),
    (None, voice_name, language) => {
      let mut voices = internal_list_voices()?
        .into_iter()
        .filter(|voice| {
          voice_name.map(|name| voice.name == name).unwrap_or(true)
            || language
              .map(|name| {
                voice.languages.is_empty()
                  || voice.languages.iter().any(|language| language == name)
              })
              .unwrap_or(true)
        })
        .filter(|voice| !audio_data_needed || voice.synthesizer.supports_speaking_to_audio_data)
        .collect::<Vec<Voice>>();
      voices.sort_unstable_by_key(|voice| voice.priority);
      voices
        .first()
        .ok_or(match (voice, language) {
          (None, None) => OutputError::NoVoices,
          (Some(voice), _) => OutputError::into_voice_not_found(voice),
          (None, Some(language)) => OutputError::into_language_not_found(language),
        })?
        .synthesizer
        .name
        .clone()
    }
  };
  Ok(synthesizer)
}
fn check_speech_parameters(
  rate: Option<u8>,
  volume: Option<u8>,
  pitch: Option<u8>,
) -> Result<(), OutputError> {
  if rate.is_some_and(|rate| rate > 100) {
    Err(OutputError::InvalidRate(rate.unwrap()))?;
  };
  if volume.is_some_and(|volume| volume > 100) {
    Err(OutputError::InvalidVolume(volume.unwrap()))?;
  };
  if pitch.is_some_and(|pitch| pitch > 100) {
    Err(OutputError::InvalidPitch(pitch.unwrap()))?;
  };
  Ok(())
}
pub fn speak_to_audio_data(
  synthesizer: Option<&str>,
  voice: Option<&str>,
  language: Option<&str>,
  rate: Option<u8>,
  volume: Option<u8>,
  pitch: Option<u8>,
  text: &str,
) -> Result<SpeechResult, OutputError> {
  check_speech_parameters(rate, volume, pitch)?;
  let synthesizer = synthesizer.map(|value| value.to_owned());
  let voice = voice.map(|value| value.to_owned());
  let language = language.map(|value| value.to_owned());
  let text = text.to_owned();
  let closure = move || {
    BACKENDS.with_borrow(|backends| {
      let synthesizer_name = filter_synthesizers(
        synthesizer.as_deref(),
        voice.as_deref(),
        language.as_deref(),
        true,
      )?;
      let synthesizer = backends
        .get(&synthesizer_name)
        .ok_or(OutputError::into_backend_not_found(&synthesizer_name))?;
      let result = match synthesizer.as_speech_synthesizer_to_audio_data() {
        None => Err(OutputError::into_audio_data_not_supported(
          &synthesizer_name,
        ))?,
        Some(synthesizer) => synthesizer.speak(
          voice.as_deref(),
          language.as_deref(),
          rate,
          volume,
          pitch,
          &text,
        )?,
      };
      Ok(Box::new(result) as OperationOk)
    })
  };
  let result = perform_operation(Box::new(closure))?
    .downcast()
    .map_err(|_| OutputError::into_unknown(anyhow!("Failed to downcast received return value")))?;
  Ok(*result)
}
pub fn speak_to_audio_output(
  synthesizer: Option<&str>,
  voice: Option<&str>,
  language: Option<&str>,
  rate: Option<u8>,
  volume: Option<u8>,
  pitch: Option<u8>,
  text: &str,
  interrupt: bool,
) -> Result<(), OutputError> {
  check_speech_parameters(rate, volume, pitch)?;
  let synthesizer = synthesizer.map(|value| value.to_owned());
  let voice = voice.map(|value| value.to_owned());
  let language = language.map(|value| value.to_owned());
  let text = text.to_owned();
  let closure = move || {
    BACKENDS.with_borrow(|backends| {
      let synthesizer_name = filter_synthesizers(
        synthesizer.as_deref(),
        voice.as_deref(),
        language.as_deref(),
        false,
      )?;
      let synthesizer = backends
        .get(&synthesizer_name)
        .ok_or(OutputError::into_backend_not_found(&synthesizer_name))?;
      match (
        synthesizer.as_speech_synthesizer_to_audio_data(),
        synthesizer.as_speech_synthesizer_to_audio_output(),
      ) {
        (None, None) => Err(OutputError::into_speech_not_supported(&synthesizer_name))?,
        (Some(synthesizer), None) => {
          let result = synthesizer.speak(
            voice.as_deref(),
            language.as_deref(),
            rate,
            volume,
            pitch,
            &text,
          )?;
          let buffer = result
            .pcm
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .collect::<Vec<i16>>();
          let source = SamplesBuffer::new(1, result.sample_rate, buffer);
          if interrupt {
            SINK
              .get()
              .ok_or(OutputError::into_unknown(anyhow!("SINK contains nothing")))?
              .stop();
          };
          SINK
            .get()
            .ok_or(OutputError::into_unknown(anyhow!("SINK contains nothing")))?
            .append(source)
        }
        (_, Some(synthesizer)) => synthesizer.speak(
          voice.as_deref(),
          language.as_deref(),
          rate,
          volume,
          pitch,
          &text,
          interrupt,
        )?,
      };
      Ok(Box::new(()) as OperationOk)
    })
  };
  perform_operation(Box::new(closure))?;
  Ok(())
}
pub fn stop_speech(synthesizer: Option<&str>) -> Result<(), OutputError> {
  let synthesizer = synthesizer.map(|value| value.to_owned());
  let closure = move || {
    BACKENDS.with_borrow(|backends| {
      match synthesizer {
        Some(synthesizer_name) => {
          let synthesizer = backends
            .get(&synthesizer_name)
            .ok_or(OutputError::into_backend_not_found(&synthesizer_name))?;
          match (
            synthesizer.as_speech_synthesizer_to_audio_data(),
            synthesizer.as_speech_synthesizer_to_audio_output(),
          ) {
            (None, None) => Err(OutputError::into_speech_not_supported(&synthesizer_name))?,
            (Some(_), None) => SINK
              .get()
              .ok_or(OutputError::into_unknown(anyhow!("SINK contains nothing")))?
              .stop(),
            (_, Some(synthesizer)) => synthesizer.stop_speech()?,
          }
        }
        None => {
          SINK
            .get()
            .ok_or(OutputError::into_unknown(anyhow!("SINK contains nothing")))?
            .stop();
          for synthesizer in backends
            .iter()
            .flat_map(|backend| backend.1.as_speech_synthesizer_to_audio_output())
          {
            let _result = synthesizer.stop_speech();
          }
        }
      };
      Ok(Box::new(()) as OperationOk)
    })
  };
  perform_operation(Box::new(closure))?;
  Ok(())
}
pub fn braille(backend: Option<&str>, text: &str) -> Result<(), OutputError> {
  let backend = backend.map(|value| value.to_owned());
  let text = text.to_owned();
  let closure = move || {
    BACKENDS.with_borrow(|backends| {
      match backend {
        Some(backend_name) => backends
          .get(&backend_name)
          .ok_or(OutputError::into_backend_not_found(&backend_name))?
          .as_braille_backend()
          .ok_or(OutputError::into_braille_not_supported(&backend_name))?
          .braille(&text)?,
        None => {
          let mut braille_backends = backends
            .iter()
            .flat_map(|backend| backend.1.as_braille_backend())
            .collect::<Vec<&dyn BrailleBackend>>();
          braille_backends.sort_unstable_by_key(|backend| backend.priority());
          braille_backends
            .first()
            .ok_or(OutputError::NoBrailleBackends)?
            .braille(&text)?
        }
      };
      Ok(Box::new(()) as OperationOk)
    })
  };
  perform_operation(Box::new(closure))?;
  Ok(())
}
pub fn output(
  synthesizer: Option<&str>,
  voice: Option<&str>,
  language: Option<&str>,
  rate: Option<u8>,
  volume: Option<u8>,
  pitch: Option<u8>,
  braille_backend: Option<&str>,
  text: &str,
  interrupt: bool,
) -> Result<(), OutputError> {
  speak_to_audio_output(
    synthesizer,
    voice,
    language,
    rate,
    volume,
    pitch,
    text,
    interrupt,
  )?;
  braille(braille_backend, text)
}
