Proof-of-Concept implementation of the paper "BlindSpot: Efficient Single-Node Selective Jamming for LoRaWAN" by Vincenz Mechler, Frank Hessel, Matthias Hollick, and Bastian Bloessl (https://doi.org/10.1145/3734477.3734724).

# Setup

Requires a FutureSDR compatible SDR device/driver.
All other dependencies are managed by cargo.
Tested with cargo version 1.86.0.

# Dataset

Download the dataset from [] and place it into the root of this project.
The individual sample files should be located in a subfolder called 'BlindSpot_dataset'.

# Usage

Run 'evaluate.rs' to transmit the blinding signal.
Provide the transmit gain and optionally a filter string to address a specific SDR device.

Without any other arguments, the script will transmit the blinding signal indefinitely.

If you provide the path to a sample file containing benign traffic (multiple files included in the dataset), the signals will be mixed and transmitted from hte same SDR, with configurable relative attenuation.

To collect the number of received frames per subchannel from the gateway, configure it to forward received frames via the semtech udp protocol to your client on port 1730 and provide the '--collect-results' flag.
The results will then be written into a csv file suitable for postprocessing with the scripts provided as artifacts to the paper.
