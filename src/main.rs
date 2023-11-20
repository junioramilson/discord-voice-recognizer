use futures::lock::Mutex;
use hound;
use serenity::http::Http;
use serenity::{
    async_trait,
    client::{Client, Context, EventHandler},
    framework::{
        standard::{
            macros::{command, group},
            Args, CommandResult,
        },
        StandardFramework,
    },
    model::{channel::Message, gateway::Ready, id::ChannelId},
    prelude::{GatewayIntents, Mentionable},
    Result as SerenityResult,
};
use songbird::{
    driver::DecodeMode,
    model::payload::{ClientDisconnect, Speaking},
    Config, CoreEvent, Event, EventContext, EventHandler as VoiceEventHandler, SerenityInit,
};
use std::collections::HashMap;
use std::env;
use std::sync::Arc;
mod audio;
mod recognize;
use recognize::{GoogleSpeechRecognizer, VoiceRecognizer};

struct Handler;

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _: Context, ready: Ready) {
        println!("{} is connected!", ready.user.name);
    }
}

struct State {
    audio_buffer_map: Arc<Mutex<HashMap<u32, Vec<i16>>>>,
    users_ssrc_map: Arc<Mutex<HashMap<u32, String>>>,
    recognizer: Arc<Mutex<dyn VoiceRecognizer + Send + Sync>>,
}

struct Receiver {
    state: Arc<Mutex<State>>,
    http: Arc<Http>,
    channel_id: ChannelId,
}

impl Receiver {
    pub fn new(state: Arc<Mutex<State>>, http: Arc<Http>, channel_id: ChannelId) -> Self {
        Self {
            state,
            http,
            channel_id,
        }
    }

    pub async fn update_audio_buffer(&self, audio: Vec<i16>, ssrc: u32) {
        let state = self.state.lock().await;

        let mut audio_buffer = state.audio_buffer_map.lock().await;
        let user_audio_buffer = audio_buffer.entry(ssrc).or_insert(Vec::new());

        user_audio_buffer.extend_from_slice(
            &audio
                .iter()
                .filter(|&x| *x != 0)
                .collect::<Vec<&i16>>()
                .iter()
                .map(|&x| *x)
                .collect::<Vec<i16>>(),
        );
    }

    pub fn write_audio_to_file(&self, ssrc: u32, audio_buffer: Vec<i16>) {
        let mut wav_writer = hound::WavWriter::create(
            format!("{}-output.wav", ssrc),
            hound::WavSpec {
                channels: 2,
                sample_rate: 44100,
                bits_per_sample: 16,
                sample_format: hound::SampleFormat::Int,
            },
        )
        .unwrap();

        for sample in audio_buffer.clone() {
            wav_writer.write_sample(sample).unwrap_or_else(|e| {
                println!("Error writing sample: {}", e);
            });
        }

        wav_writer.finalize().unwrap_or_else(|e| {
            println!("Error finalizing wav file: {}", e);
        });
    }
}

#[async_trait]
impl VoiceEventHandler for Receiver {
    #[allow(unused_variables)]
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        use EventContext as Ctx;
        match ctx {
            Ctx::SpeakingStateUpdate(Speaking {
                speaking,
                ssrc,
                user_id,
                ..
            }) => {
                // Discord voice calls use RTP, where every sender uses a randomly allocated
                // *Synchronisation Source* (SSRC) to allow receivers to tell which audio
                // stream a received packet belongs to. As this number is not derived from
                // the sender's user_id, only Discord Voice Gateway messages like this one
                // inform us about which random SSRC a user has been allocated. Future voice
                // packets will contain *only* the SSRC.
                //
                // You can implement logic here so that you can differentiate users'
                // SSRCs and map the SSRC to the User ID and maintain this state.
                // Using this map, you can map the `ssrc` in `voice_packet`
                // to the user ID and handle their audio packets separately.
                println!(
                    "Speaking state update: user {:?} has SSRC {:?}, using {:?}",
                    user_id, ssrc, speaking,
                );

                self.state
                    .lock()
                    .await
                    .users_ssrc_map
                    .lock()
                    .await
                    .insert(*ssrc, user_id.clone().unwrap().0.to_string());
            }
            Ctx::SpeakingUpdate(data) => match data.speaking {
                true => {
                    println!("User {} started speaking", data.ssrc);
                }
                false => {
                    println!("User {} stopped speaking", data.ssrc);

                    let state = self.state.lock().await;
                    let mut audio_buffer = state.audio_buffer_map.lock().await;
                    let user_audio_buffer = audio_buffer.get(&data.ssrc).unwrap().clone();

                    if user_audio_buffer.len() == 0 {
                        return None;
                    }

                    println!("Writing audio to file...");
                    self.write_audio_to_file(data.ssrc, user_audio_buffer.clone());

                    println!("Generating transcript for {}", data.ssrc);
                    let file = std::fs::read(format!("{}-output.wav", data.ssrc)).unwrap();
                    std::fs::remove_file(format!("{}-output.wav", data.ssrc)).unwrap();

                    let recognizer = state.recognizer.lock().await;
                    let transcription = recognizer.execute(file).await;

                    if let Some(transcription) = transcription {
                        let channel_id = self.channel_id;

                        check_msg(
                            channel_id
                                .say(
                                    &self.http,
                                    &format!("{} disse: {}", data.ssrc, transcription),
                                )
                                .await,
                        );
                    } else {
                        println!("Failed to transcribe audio");
                        return None;
                    }

                    println!("Removing audio buffer for {}", data.ssrc);
                    audio_buffer.remove(&data.ssrc);
                }
            },
            Ctx::VoicePacket(data) => {
                // An event which fires for every received audio packet,
                // containing the decoded data.
                if let Some(audio) = data.audio.clone() {
                    println!(
                        "Audio packet's first 5 samples: {:?}",
                        audio.get(..5.min(audio.len()))
                    );
                    println!(
                        "Audio packet sequence {:05} has {:04} bytes (decompressed from {}), SSRC {}",
                        data.packet.sequence.0,
                        audio.len() * std::mem::size_of::<i16>(),
                        data.packet.payload.len(),
                        data.packet.ssrc,
                    );

                    println!("Audio length: {}", audio.len());

                    self.update_audio_buffer(audio, data.packet.ssrc).await;
                } else {
                    println!("RTP packet, but no audio. Driver may not be configured to decode.");
                }
            }
            Ctx::RtcpPacket(data) => {
                // An event which fires for every received rtcp packet,
                // containing the call statistics and reporting information.
                println!("RTCP packet received: {:?}", data.packet);
            }
            Ctx::ClientDisconnect(ClientDisconnect { user_id, .. }) => {
                // You can implement your own logic here to handle a user who has left the
                // voice channel e.g., finalise processing of statistics etc.
                // You will typically need to map the User ID to their SSRC; observed when
                // first speaking.

                println!("Client disconnected: user {:?}", user_id);
            }
            _ => {
                // We won't be registering this struct for any more event classes.
                unimplemented!()
            }
        }

        None
    }
}

#[group]
#[commands(join, leave, ping)]
struct General;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();
    dotenv::dotenv().expect("Failed to load .env file");

    let discord_token = env::var("DISCORD_TOKEN").expect("Expected a token in the environment");

    let framework = StandardFramework::new()
        .configure(|c| c.prefix("!"))
        .group(&GENERAL_GROUP);

    let intents = GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT;

    let songbird_config = Config::default().decode_mode(DecodeMode::Decode);

    let mut client = Client::builder(&discord_token, intents)
        .event_handler(Handler)
        .framework(framework)
        .register_songbird_from_config(songbird_config)
        .await
        .expect("Err creating client");

    let _ = client
        .start()
        .await
        .map_err(|why| println!("Client ended: {:?}", why));

    tokio::spawn(async move {
        let _ = client
            .start()
            .await
            .map_err(|why| println!("Client ended: {:?}", why));
    });
}

#[command]
#[only_in(guilds)]
async fn join(ctx: &Context, msg: &Message, mut args: Args) -> CommandResult {
    let connect_to = match args.single::<u64>() {
        Ok(id) => ChannelId(id),
        Err(_) => {
            check_msg(
                msg.reply(ctx, "Requires a valid voice channel ID be given")
                    .await,
            );

            return Ok(());
        }
    };

    let guild = msg.guild(&ctx.cache).unwrap();
    let guild_id = guild.id;

    let manager = songbird::get(ctx)
        .await
        .expect("Songbird Voice client placed in at initialisation.")
        .clone();

    let (handler_lock, conn_result) = manager.join(guild_id, connect_to).await;

    if let Ok(_) = conn_result {
        let audio_buffer = Arc::new(Mutex::new(HashMap::new()));
        let users_ssrc_map = Arc::new(Mutex::new(HashMap::<u32, String>::new()));
        let google_recognizer = Arc::new(Mutex::new(GoogleSpeechRecognizer {}));

        let http = ctx.http.clone();
        let channel_id = msg.channel_id;

        let state = Arc::new(Mutex::new(State {
            audio_buffer_map: audio_buffer.clone(),
            users_ssrc_map: users_ssrc_map.clone(),
            recognizer: google_recognizer.clone(),
        }));

        let mut handler = handler_lock.lock().await;
        handler.add_global_event(
            CoreEvent::SpeakingStateUpdate.into(),
            Receiver::new(state.clone(), http.clone(), channel_id),
        );
        handler.add_global_event(
            CoreEvent::SpeakingUpdate.into(),
            Receiver::new(state.clone(), http.clone(), channel_id),
        );
        handler.add_global_event(
            CoreEvent::VoicePacket.into(),
            Receiver::new(state.clone(), http.clone(), channel_id),
        );
        handler.add_global_event(
            CoreEvent::RtcpPacket.into(),
            Receiver::new(state.clone(), http.clone(), channel_id),
        );
        handler.add_global_event(
            CoreEvent::ClientDisconnect.into(),
            Receiver::new(state.clone(), http.clone(), channel_id),
        );

        check_msg(
            msg.channel_id
                .say(&ctx.http, &format!("Joined {}", connect_to.mention()))
                .await,
        );
    } else {
        check_msg(
            msg.channel_id
                .say(&ctx.http, "Error joining the channel")
                .await,
        );
    }

    Ok(())
}

#[command]
#[only_in(guilds)]
async fn leave(ctx: &Context, msg: &Message) -> CommandResult {
    let guild = msg.guild(&ctx.cache).unwrap();
    let guild_id = guild.id;

    let manager = songbird::get(ctx)
        .await
        .expect("Songbird Voice client placed in at initialisation.")
        .clone();
    let has_handler = manager.get(guild_id).is_some();

    if has_handler {
        if let Err(e) = manager.remove(guild_id).await {
            check_msg(
                msg.channel_id
                    .say(&ctx.http, format!("Failed: {:?}", e))
                    .await,
            );
        }

        check_msg(msg.channel_id.say(&ctx.http, "Left voice channel").await);
    } else {
        check_msg(msg.reply(ctx, "Not in a voice channel").await);
    }

    Ok(())
}

#[command]
async fn ping(ctx: &Context, msg: &Message) -> CommandResult {
    check_msg(msg.channel_id.say(&ctx.http, "Pong!").await);

    Ok(())
}

fn check_msg(result: SerenityResult<Message>) {
    if let Err(why) = result {
        println!("Error sending message: {:?}", why);
    }
}
