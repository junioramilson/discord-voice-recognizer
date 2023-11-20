use base64::prelude::*;
use serde::Deserialize;
use serenity::async_trait;
use serenity::json::json;
use std::env;

#[async_trait]
pub trait VoiceRecognizer {
    async fn execute(&self, data: Vec<u8>) -> Option<String>;
}

pub struct GoogleSpeechRecognizer;

#[derive(Deserialize, Debug)]
pub struct GoogleSpeechAlternative {
    pub transcript: String,
    pub confidence: f32,
}

#[derive(Deserialize, Debug)]
pub struct GoogleSpeechResult {
    pub alternatives: Vec<GoogleSpeechAlternative>,
}

#[derive(Deserialize, Debug)]
pub struct GoogleSpeechSuccessResponse {
    pub results: Vec<GoogleSpeechResult>,
}

impl GoogleSpeechSuccessResponse {
    pub fn get_best_alternative(&self) -> Option<&GoogleSpeechAlternative> {
        self.results.first()?.alternatives
            .iter()
            .max_by(|a, b| a.confidence.partial_cmp(&b.confidence).unwrap())
    }
}
#[async_trait]
impl VoiceRecognizer for GoogleSpeechRecognizer {
    async fn execute(&self, audio_buffer: Vec<u8>) -> Option<String> {
        println!("GoogleSpeech Request ongoing...");

        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "X-goog-api-key",
            env::var("GOOGLE_API_KEY").unwrap().parse().unwrap(),
        );

        let encoded_b64 = base64::engine::general_purpose::STANDARD_NO_PAD.encode(audio_buffer);
        let request_body = reqwest::Body::from(
            json!({
                "config": {
                    "sampleRateHertz": 44100,
                    "languageCode": "pt-BR",
                    "audioChannelCount": 1,
                },
                "audio": {
                    "content": encoded_b64
                }
            })
                .to_string(),
        );
        let response = reqwest::Client::new()
            .post("https://speech.googleapis.com/v1/speech:recognize")
            .headers(headers)
            .body(request_body)
            .send()
            .await;

        let response = response.unwrap_or_else(|e| {
            println!("Error sending request: {}", e);
            panic!();
        });

        let response_json = response.json::<serde_json::Value>().await;
        println!("GoogleSpeech Request full response: {:?}", response_json);

        let speech_response = serde_json::from_value::<Result<GoogleSpeechSuccessResponse, ()>>(
            response_json.unwrap(),
        )
            .unwrap_or_else(|e| {
                println!("Error deserializing response: {}", e);
                return Err(());
            });

        match speech_response {
            Ok(success_response) => {
                let best_alternative = success_response
                    .get_best_alternative()
                    .expect("Failed to get best alternative");
                println!("Best alternative: {:?}", best_alternative);

                let transcription = best_alternative.transcript.to_string();
                println!("Transcription: {}", transcription);

                Some(transcription)
            }
            Err(_) => {
                println!("Error deserializing response");
                None
            }
        }
    }
}
