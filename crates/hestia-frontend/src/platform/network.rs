//! Android network selection for LAN hosts.

use std::net::{IpAddr, ToSocketAddrs};

use hestia_core::stream::{SessionUrl, StreamConfig};
use jni::JNIEnv;
use jni::objects::{JByteArray, JObject, JValue};

use super::android::with_activity;

const TRANSPORT_CELLULAR: i32 = 0;
const TRANSPORT_VPN: i32 = 4;
const NET_CAPABILITY_NOT_METERED: i32 = 11;
const NET_CAPABILITY_NOT_VPN: i32 = 15;
const NETWORK_SIG: &str = "(Landroid/net/Network;)";

fn connectivity_manager<'local>(
    env: &mut JNIEnv<'local>,
    activity: &JObject<'_>,
) -> jni::errors::Result<JObject<'local>> {
    let name = env.new_string("connectivity")?;
    env.call_method(
        activity,
        "getSystemService",
        "(Ljava/lang/String;)Ljava/lang/Object;",
        &[JValue::Object(&name)],
    )?
    .l()
}

fn stream_network<'local>(env: &mut JNIEnv<'local>, cm: &JObject<'_>) -> jni::errors::Result<JObject<'local>> {
    // Honor an explicit Wi-Fi binding even when the default is cellular.
    let network = env
        .call_method(cm, "getBoundNetworkForProcess", "()Landroid/net/Network;", &[])?
        .l()?;
    if network.is_null() {
        env.call_method(cm, "getActiveNetwork", "()Landroid/net/Network;", &[])?
            .l()
    } else {
        Ok(network)
    }
}

fn network_capabilities<'local>(
    env: &mut JNIEnv<'local>,
    cm: &JObject<'_>,
    network: &JObject<'_>,
) -> jni::errors::Result<JObject<'local>> {
    if network.is_null() {
        return Ok(JObject::null());
    }
    env.call_method(
        cm,
        "getNetworkCapabilities",
        format!("{NETWORK_SIG}Landroid/net/NetworkCapabilities;"),
        &[JValue::Object(network)],
    )?
    .l()
}

/// Compare the RTSP destination with the network's DNS64 synthesis prefix.
fn uses_nat64(
    env: &mut JNIEnv<'_>,
    cm: &JObject<'_>,
    network: &JObject<'_>,
    url: &SessionUrl,
) -> jni::errors::Result<bool> {
    if network.is_null() || env.get_static_field("android/os/Build$VERSION", "SDK_INT", "I")?.i()? < 30 {
        return Ok(false);
    }
    let link = env
        .call_method(
            cm,
            "getLinkProperties",
            format!("{NETWORK_SIG}Landroid/net/LinkProperties;"),
            &[JValue::Object(network)],
        )?
        .l()?;
    if link.is_null() {
        return Ok(false);
    }
    let prefix = env
        .call_method(&link, "getNat64Prefix", "()Landroid/net/IpPrefix;", &[])?
        .l()?;
    if prefix.is_null() {
        return Ok(false);
    }
    let target = url.host.parse::<IpAddr>().ok().or_else(|| {
        (url.host.as_str(), url.port)
            .to_socket_addrs()
            .ok()?
            .next()
            .map(|a| a.ip())
    });
    let Some(IpAddr::V6(target)) = target else {
        return Ok(false);
    };
    let octets = env.byte_array_from_slice(&target.octets())?;
    let address = env
        .call_static_method(
            "java/net/InetAddress",
            "getByAddress",
            "([B)Ljava/net/InetAddress;",
            &[JValue::Object(&octets)],
        )?
        .l()?;
    env.call_method(
        &prefix,
        "contains",
        "(Ljava/net/InetAddress;)Z",
        &[JValue::Object(&address)],
    )?
    .z()
}

pub(crate) fn stream_network_metered() -> Option<bool> {
    let mut metered = None;
    if let Err(e) = with_activity(|env, activity| {
        let cm = connectivity_manager(env, activity)?;
        let network = stream_network(env, &cm)?;
        let caps = network_capabilities(env, &cm, &network)?;
        if !caps.is_null() {
            metered = Some(
                !env.call_method(
                    &caps,
                    "hasCapability",
                    "(I)Z",
                    &[JValue::Int(NET_CAPABILITY_NOT_METERED)],
                )?
                .z()?,
            );
        }
        Ok(())
    }) {
        log::debug!("could not check Android network metering: {e}");
    }
    metered
}

/// Use WAN packet size and QoS on Android VPNs, cellular and NAT64 paths.
pub(crate) fn configure_stream_network(mut config: StreamConfig) -> StreamConfig {
    if config.streaming_remotely {
        return config;
    }
    let mut remote = false;
    let result = with_activity(|env, activity| {
        let cm = connectivity_manager(env, activity)?;
        let network = stream_network(env, &cm)?;
        let caps = network_capabilities(env, &cm, &network)?;
        if !caps.is_null() {
            remote = env
                .call_method(&caps, "hasTransport", "(I)Z", &[JValue::Int(TRANSPORT_CELLULAR)])?
                .z()?
                || env
                    .call_method(&caps, "hasTransport", "(I)Z", &[JValue::Int(TRANSPORT_VPN)])?
                    .z()?
                || !env
                    .call_method(&caps, "hasCapability", "(I)Z", &[JValue::Int(NET_CAPABILITY_NOT_VPN)])?
                    .z()?;
        }
        if !remote {
            remote = uses_nat64(env, &cm, &network, &config.session_url)?;
        }
        Ok(())
    });
    if let Err(e) = result {
        log::debug!("could not determine Android stream network: {e}");
    } else if remote {
        config.streaming_remotely = true;
        config.packet_size = StreamConfig::WAN_PACKET_SIZE;
        log::info!(
            "Android stream uses cellular, VPN or NAT64; using WAN packet size {}",
            config.packet_size
        );
    }
    config
}

/// Called before each connection to a host (see [`crate::netroute`]): binds
/// the app to the Wi-Fi/Ethernet network that holds a LAN address when that
/// network isn't Android's default (Wi-Fi without internet → mobile data is
/// the default and LAN connections would time out). Checked at most every
/// few seconds per address.
pub fn route_for(addr: &std::net::SocketAddr) {
    use std::net::IpAddr;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static LAST: Mutex<Option<(IpAddr, Instant)>> = Mutex::new(None);
    let ip = addr.ip();
    {
        let mut last = LAST.lock().unwrap_or_else(|p| p.into_inner());
        if let Some((prev, at)) = *last
            && prev == ip
            && at.elapsed() < Duration::from_secs(10)
        {
            return;
        }
        *last = Some((ip, Instant::now()));
    }
    if let Err(e) = apply_route(ip) {
        log::warn!("choosing the network for {ip} failed: {e}");
    }
}

fn apply_route(ip: std::net::IpAddr) -> Result<(), String> {
    use jni::objects::JObjectArray;

    use crate::netroute::{self, NetInfo};

    with_activity(|env, activity| {
        let cm = connectivity_manager(env, activity)?;
        let active = env
            .call_method(&cm, "getActiveNetwork", "()Landroid/net/Network;", &[])?
            .l()?;
        let all = JObjectArray::from(
            env.call_method(&cm, "getAllNetworks", "()[Landroid/net/Network;", &[])?
                .l()?,
        );
        let mut infos = Vec::new();
        let mut handles = Vec::new();
        for i in 0..env.get_array_length(&all)? {
            let net = env.get_object_array_element(&all, i)?;
            let caps = env
                .call_method(
                    &cm,
                    "getNetworkCapabilities",
                    format!("{NETWORK_SIG}Landroid/net/NetworkCapabilities;"),
                    &[JValue::Object(&net)],
                )?
                .l()?;
            let props = env
                .call_method(
                    &cm,
                    "getLinkProperties",
                    format!("{NETWORK_SIG}Landroid/net/LinkProperties;"),
                    &[JValue::Object(&net)],
                )?
                .l()?;
            if caps.is_null() || props.is_null() {
                continue;
            }
            let cellular = env
                .call_method(&caps, "hasTransport", "(I)Z", &[JValue::Int(TRANSPORT_CELLULAR)])?
                .z()?;
            let vpn = env
                .call_method(&caps, "hasTransport", "(I)Z", &[JValue::Int(TRANSPORT_VPN)])?
                .z()?;
            let list = env
                .call_method(&props, "getLinkAddresses", "()Ljava/util/List;", &[])?
                .l()?;
            let mut addresses = Vec::new();
            for j in 0..env.call_method(&list, "size", "()I", &[])?.i()? {
                let link = env
                    .call_method(&list, "get", "(I)Ljava/lang/Object;", &[JValue::Int(j)])?
                    .l()?;
                let inet = env
                    .call_method(&link, "getAddress", "()Ljava/net/InetAddress;", &[])?
                    .l()?;
                let prefix = env.call_method(&link, "getPrefixLength", "()I", &[])?.i()?;
                let raw = JByteArray::from(env.call_method(&inet, "getAddress", "()[B", &[])?.l()?);
                if let Some(address) = netroute::ip_from_bytes(&env.convert_byte_array(&raw)?) {
                    addresses.push((address, prefix.clamp(0, 128) as u8));
                }
                env.delete_local_ref(raw)?;
                env.delete_local_ref(inet)?;
                env.delete_local_ref(link)?;
            }
            let is_active = !active.is_null()
                && env
                    .call_method(&net, "equals", "(Ljava/lang/Object;)Z", &[JValue::Object(&active)])?
                    .z()?;
            infos.push(NetInfo {
                local_transport: !cellular && !vpn,
                active: is_active,
                addresses,
            });
            handles.push(net);
        }

        let null = JObject::null();
        let choice = netroute::choose(ip, &infos);
        let target = choice.map_or(&null, |i| &handles[i]);
        let bound = env
            .call_method(&cm, "getBoundNetworkForProcess", "()Landroid/net/Network;", &[])?
            .l()?;
        let unchanged = match (bound.is_null(), target.is_null()) {
            (true, true) => true,
            (false, false) => env
                .call_method(&bound, "equals", "(Ljava/lang/Object;)Z", &[JValue::Object(target)])?
                .z()?,
            _ => false,
        };
        if !unchanged {
            let ok = env
                .call_method(
                    &cm,
                    "bindProcessToNetwork",
                    format!("{NETWORK_SIG}Z"),
                    &[JValue::Object(target)],
                )?
                .z()?;
            match choice {
                Some(i) => log::info!(
                    "{ip} is on a local network that isn't Android's default (Wi-Fi without internet?); \
                     routing the app through it ({:?}): {}",
                    infos[i].addresses,
                    if ok { "ok" } else { "refused" }
                ),
                None => log::info!("routing back through Android's default network for {ip}"),
            }
        }
        Ok(())
    })
}
