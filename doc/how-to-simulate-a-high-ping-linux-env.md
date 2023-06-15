# How to simulate a high ping Linux environment

To simulate a high ping Linux environment, you can use network simulation tools to increase network latency and packet loss. Here are the basic steps to help you achieve this:

1. Use the `tc` command to configure network latency and packet loss. `tc` is a network management tool on Linux that allows you to simulate network conditions. You can install the `tc` tool (if not already installed) using the following command:

```bash
sudo dnf install -y iproute-tc
```

2. Use the following command to add delay and packet loss:

```bash
sudo tc qdisc add dev <interface> root netem delay <delay>ms loss <loss>%
```

Where
- `<interface>` is the network interface on which you want to simulate the network. For example, enp0s3.
- `<delay>` is the delay time in milliseconds, You can set an appropriate value to simulate high latency.
- `<loss>` is the percentage of packet loss. You can ste an appropriate value to simulate packet loss.

3. Now, your network interface should simulate high latency and packet loss. You can use the `ping` command to test network latency:

```bash
ping <destination>
```

_(For Fedora)_ You may encounter the error "Error: Specified qdisc kind is unknown" while using the `tc` command, it is likely because the required module is not loaded in your linux kernel or not supported by your kernel.

To provide support for the `netem` module, you need to install the `kernel-modules-extra` package:

```bash
sudo dnf install -y kernel-modules-extra
```

After installing the kernel-modules-extra package, you will need to restart your system for the changes to take effect.
