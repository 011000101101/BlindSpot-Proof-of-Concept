use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::str::FromStr;
use std::task::Context;
use std::task::Poll;
use std::time::Duration;

use semtech_udp::CodingRate;
use semtech_udp::DataRate;
use semtech_udp::MacAddress;
use semtech_udp::Modulation;
use semtech_udp::SpreadingFactor;
use semtech_udp::pull_resp::PhyData;
use semtech_udp::pull_resp::Time;
use semtech_udp::pull_resp::TxPk;
use semtech_udp::server_runtime::Error;
use semtech_udp::server_runtime::Event;
use semtech_udp::server_runtime::UdpRuntime;
use semtech_udp::tx_ack;
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

use futuresdr::futures::FutureExt;
use futuresdr::futures::SinkExt;
use futuresdr::futures::channel::mpsc;
use futuresdr::futures::channel::mpsc::UnboundedReceiver;
use futuresdr::futures::channel::oneshot;
use futuresdr::futures::channel::oneshot::Sender;
use futuresdr::futures::future::Either;
use futuresdr::futures::future::select;
use futuresdr::futures::pin_mut;
use futuresdr::futures::select;
use futuresdr::futures_lite::StreamExt;
use futuresdr::tracing::{debug, info, warn};

fn freq_to_chan_nr(freq: usize) -> usize {
    match freq {
        867900000 => 0,
        868100000 => 1,
        868300000 => 2,
        868500000 => 3,
        867100000 => 4,
        867300000 => 5,
        867500000 => 6,
        867700000 => 7,
        _ => panic!("invalid channel center frequency"),
    }
}

struct DownlinkRequest {
    payload: Vec<u8>,
    _sf: SpreadingFactor,
    channel: usize,
    _cr: CodingRate,
    preamb_count: usize,
}

pub struct UdpServerFrameCounter {
    _runtime: Runtime,
    receiver_join_handle: JoinHandle<()>,
    result_receiver: UnboundedReceiver<(usize, usize)>,
    finished_sender: Option<Sender<()>>,
    connected_receiver: Option<oneshot::Receiver<()>>,
    downlink_sender: mpsc::UnboundedSender<DownlinkRequest>,
}

struct UdpReceiverPoller {
    poller: Pin<Box<dyn Future<Output = (Option<Event>, UdpRuntime)> + Send>>,
    interrupt_sender: Option<Sender<()>>,
}

impl UdpReceiverPoller {
    pub fn new(udp_runtime: UdpRuntime, gw_connected: bool) -> UdpReceiverPoller {
        let (interrupt_sender, interrupt_receiver) = oneshot::channel();
        UdpReceiverPoller {
            poller: Box::pin(Self::helper(udp_runtime, interrupt_receiver, gw_connected)),
            interrupt_sender: Some(interrupt_sender),
        }
    }

    pub fn cancel(&mut self) {
        self.interrupt_sender.take().unwrap().send(()).unwrap();
    }

    async fn helper(
        mut udp_runtime: UdpRuntime,
        interrupt_receiver: oneshot::Receiver<()>,
        gw_connected: bool,
    ) -> (Option<Event>, UdpRuntime) {
        let result = if gw_connected {
            let fut1 = udp_runtime.recv().fuse();
            let fut2 = interrupt_receiver.fuse();

            {
                pin_mut!(fut1, fut2);

                select! {
                    res = fut1 => Some(res),
                    _ = fut2 => None,
                }
            }
        } else {
            Some(udp_runtime.recv().await)
        };

        (result, udp_runtime)
    }
}

impl Future for UdpReceiverPoller {
    type Output = (Option<Event>, UdpRuntime);

    fn poll(self: Pin<&mut Self>, cx: &mut Context) -> Poll<Self::Output> {
        Pin::new(&mut self.get_mut().poller).poll(cx)
    }
}

impl Unpin for UdpReceiverPoller {}

impl UdpServerFrameCounter {
    pub fn new(port: u16) -> Self {
        let (finished_sender, finished_receiver) = oneshot::channel::<()>();
        let (connected_sender, connected_receiver) = oneshot::channel::<()>();
        let mut connected_sender = Some(connected_sender);
        let (mut result_sender, result_receiver) = mpsc::unbounded::<(usize, usize)>();
        let (downlink_sender, mut downlink_receiver) = mpsc::unbounded::<DownlinkRequest>();
        let rt_tokio = Runtime::new().unwrap();
        let receiver_join_handle = rt_tokio.spawn(async move {
            let addr = SocketAddr::from(([0, 0, 0, 0], port));
            debug!("Starting server: {addr}");
            let mut udp_runtime = UdpRuntime::new(addr).await.unwrap_or_else( |e| panic!(
                "Binding to UDP Port {addr} failed, another instance is possibly already running.\noriginal Error: {}", e
            )
            );
            let mut finished_receiver_pinned = Box::pin(finished_receiver);
            let mut downlink_receiver_pinned = None;
            let mut udp_runtime_receiver_pinned =
                Some(Box::pin(UdpReceiverPoller::new(udp_runtime, false)));
            debug!("Ready for clients");

            let mut downlink_requests: Vec<DownlinkRequest> = vec![];
            let mut gw_mac: Option<MacAddress> = None;
            loop {
                debug!("Waiting for event");
                if downlink_receiver_pinned.is_none() {
                    downlink_receiver_pinned = Some(Box::pin(downlink_receiver.next()));
                }
                match select(
                    finished_receiver_pinned,
                    select(
                        downlink_receiver_pinned.take().unwrap(),
                        udp_runtime_receiver_pinned.take().unwrap(),
                    ),
                )
                .await
                {
                    Either::Left(_) => {
                        break;
                    }
                    Either::Right((
                        Either::Left((downlink_request, mut udp_runtime_receiver_pinned_unused)),
                        terminated_receiver_pinned_unused,
                    )) => {
                        finished_receiver_pinned = terminated_receiver_pinned_unused;
                        downlink_requests.push(downlink_request.unwrap());
                        println!("initiating Send.");
                        udp_runtime_receiver_pinned_unused.cancel();
                        udp_runtime_receiver_pinned = Some(udp_runtime_receiver_pinned_unused);
                    }
                    Either::Right((
                        Either::Right(((packet, mut runtime), downlink_receiver_pinned_unused)),
                        terminated_receiver_pinned_unused,
                    )) => {
                        finished_receiver_pinned = terminated_receiver_pinned_unused;
                        downlink_receiver_pinned = Some(downlink_receiver_pinned_unused);
                        let mut gw_connected = false;
                        if let Some(actual_packet) = packet {
                            match actual_packet {
                                Event::UnableToParseUdpFrame(error, buf) => {
                                    debug!("Semtech UDP Parsing Error: {error}");
                                    debug!("UDP data: {buf:?}");
                                }
                                Event::NewClient((mac, addr)) => {
                                    println!("New packet forwarder client: {mac}, {addr}");
                                    gw_mac = Some(mac);
                                    gw_connected = true;
                                    if let Some(sender) = connected_sender.take() {
                                        // notify the adapter of the first connection to enable the client to wait until the connector is ready
                                        sender.send(()).unwrap();
                                    }
                                }
                                Event::UpdateClient((mac, addr)) => {
                                    debug!("Mac existed, but IP updated: {mac}, {addr}");
                                }
                                Event::ClientDisconnected((mac, addr)) => {
                                    debug!("Client disconnected: {mac}, {addr}");
                                    gw_mac = Some(mac);
                                    gw_connected = false;
                                }
                                Event::PacketReceived(rxpk, _) => 'received: {
                                    debug!(
                                        "RECEIVED FRAME: {}",
                                        String::from_utf8_lossy(rxpk.get_data())
                                    );
                                    let chan: f32 =
                                        rxpk.get_frequency().to_string().parse().unwrap();
                                    let chan = ((chan * 10.) as usize) * 100000;
                                    let chan = freq_to_chan_nr(chan);
                                    let bw = rxpk
                                        .get_datarate()
                                        .to_string()
                                        .split('W')
                                        .nth(1)
                                        .unwrap()
                                        .to_string();
                                    let sf = rxpk
                                        .get_datarate()
                                        .to_string()
                                        .split('B')
                                        .next()
                                        .unwrap()
                                        .split('F')
                                        .nth(1)
                                        .unwrap()
                                        .to_string();
                                    if bw != "125" {
                                        warn!("got RX Packet with BW {bw}, SF {sf}, channel {chan}\n{:?}\n{}",rxpk.get_data(), String::from_utf8_lossy(rxpk.get_data()));
                                        break 'received;   // TODO might want to count these separately..
                                    }
                                    result_sender
                                        .send((chan, sf.parse().unwrap()))
                                        .await
                                        .unwrap();
                                }
                                Event::StatReceived(stat, gateway_mac) => {
                                    debug!("From {gateway_mac}: {stat:?}");
                                }
                                Event::NoClientWithMac(_packet, mac) => {
                                    debug!("Tried to send to client with unknown MAC: {mac:?}")
                                }
                            }
                        } else {
                            info!("commencing Send.");
                            // process queued downlink requests
                            for dl_req in &downlink_requests {
                                let txpk = TxPk {
                                    time: Time::immediate(),
                                    freq: (Into::<usize>::into(dl_req.channel) / 100000) as f64
                                        / 10.,
                                    rfch: 0,
                                    powe: 0,
                                    modu: Modulation::LORA,
                                    datr: DataRate::new(
                                        semtech_udp::SpreadingFactor::from_str(&dl_req._sf.to_string()).unwrap(),
                                        semtech_udp::Bandwidth::BW125,
                                    ),
                                    codr: CodingRate::_4_5,
                                    fdev: None,
                                    ipol: false,
                                    prea: Some(dl_req.preamb_count as u64),
                                    data: PhyData::new(dl_req.payload.clone()),
                                    ncrc: None,
                                };
                                let prepared_send = runtime.prepare_downlink(txpk, gw_mac.unwrap());
                                // tokio::spawn(async move {
                                if let Err(e) =
                                    prepared_send.dispatch(Some(Duration::from_secs(5))).await
                                {
                                    if let Error::Ack(tx_ack::Error::AdjustedTransmitPower(
                                        adjusted_power,
                                        _tmst,
                                    )) = e
                                    {
                                        // Generally, all packet forwarders will reduce output power to appropriate levels.
                                        // Packet forwarder may optionally indicate the actual power used.
                                        info!(
                                            "Packet sent at adjusted power: {adjusted_power:?}"
                                        )
                                    } else {
                                        warn!("Transmit Dispatch threw error: {e:?}")
                                    }
                                } else {
                                    info!("Send complete");
                                }
                                // });
                            }
                        }
                        udp_runtime = runtime;
                        udp_runtime_receiver_pinned =
                            Some(Box::pin(UdpReceiverPoller::new(udp_runtime, gw_connected)));
                    }
                };
            }
        });
        UdpServerFrameCounter {
            _runtime: rt_tokio,
            receiver_join_handle,
            result_receiver,
            finished_sender: Some(finished_sender),
            connected_receiver: Some(connected_receiver),
            downlink_sender,
        }
    }

    pub fn terminate(&mut self) {
        self.finished_sender.take().unwrap().send(()).unwrap();
    }

    pub async fn transmit_downlink(
        &mut self,
        msg: &str,
        sf: SpreadingFactor,
        chan: usize,
        cr: CodingRate,
        preamb_count: Option<usize>,
    ) {
        self.downlink_sender
            .send(DownlinkRequest {
                payload: Vec::from(msg.as_bytes()),
                _sf: sf,
                channel: chan,
                _cr: cr,
                preamb_count: preamb_count.unwrap_or(8),
            })
            .await
            .unwrap();
    }

    pub async fn wait_until_connected(mut self) -> Self {
        self.connected_receiver.take().unwrap_or_else(|| panic!("wait_until_connected() must only be called once, but has been called a second time.")).await.unwrap();
        self
    }

    pub async fn terminate_and_accumulate_results(
        mut self,
        num_channels: usize,
        num_sf: usize,
    ) -> Vec<Vec<usize>> {
        self.terminate();
        self.receiver_join_handle.await.unwrap();
        let mut received: Vec<Vec<usize>> = vec![vec![0; num_channels]; num_sf];
        self.result_receiver
            .for_each(|(channel, sf)| {
                if sf >= 5 && sf <= 12 {
                    received[sf - 5][channel] += 1;
                } else {
                    warn!("Hardware GW received Frame on invalid SF: {sf}");
                }
            })
            .await;
        received
    }
}
