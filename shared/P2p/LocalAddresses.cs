using System;
using System.Collections.Generic;
using System.Net;
using System.Net.NetworkInformation;
using System.Net.Sockets;

namespace HdrCast.P2p
{
    /// <summary>One unicast address of this PC as the OS reports it.</summary>
    public sealed class AddressCandidate
    {
        public AddressCandidate(IPAddress address, string interfaceId, bool temporary, bool preferred, long preferredLifetimeSeconds)
        {
            Address = address; InterfaceId = interfaceId; Temporary = temporary; Preferred = preferred;
            PreferredLifetimeSeconds = preferredLifetimeSeconds;
        }

        public IPAddress Address { get; }
        public string InterfaceId { get; }
        /// <summary>A privacy (RFC 4941) address: Windows reports SuffixOrigin Random for these.</summary>
        public bool Temporary { get; }
        /// <summary>DAD state Preferred (deprecated / tentative addresses are not).</summary>
        public bool Preferred { get; }
        public long PreferredLifetimeSeconds { get; }
    }

    /// <summary>
    /// Which addresses to bind and advertise (docs/design.md §8.7). The route source for a global destination
    /// tells us the interface; on it we prefer the stable address, because Windows sends from its temporary
    /// address by default and a firewall pinhole opened from one address does not cover the other.
    /// </summary>
    public static class LocalAddresses
    {
        static readonly IPAddress ProbeV6 = IPAddress.Parse("2001:4860:4860::8888");
        static readonly IPAddress ProbeV4 = IPAddress.Parse("8.8.8.8");

        /// <summary>The address this PC would send from for an internet destination (UDP connect, nothing is sent).</summary>
        public static IPAddress? RouteSource(AddressFamily family)
        {
            try
            {
                using (var s = new Socket(family, SocketType.Dgram, ProtocolType.Udp))
                {
                    s.Connect(new IPEndPoint(family == AddressFamily.InterNetworkV6 ? ProbeV6 : ProbeV4, 53));
                    return ((IPEndPoint)s.LocalEndPoint!).Address;
                }
            }
            catch (Exception)
            {
                return null;       // no route (no IPv6 at all), or IPv6 disabled
            }
        }

        /// <summary>This PC's stable global IPv6 address, or null when it has no IPv6 internet route.</summary>
        public static IPAddress? BestIpv6()
        {
            IPAddress? source = RouteSource(AddressFamily.InterNetworkV6);
            if (source == null || !IsGlobalUnicastV6(source)) return null;
            return PickStableIpv6(source, Read());
        }

        /// <summary>The LAN (or public) IPv4 address used for the internet route; null without IPv4.</summary>
        public static IPAddress? LanIpv4()
        {
            IPAddress? a = RouteSource(AddressFamily.InterNetwork);
            return a == null || IPAddress.IsLoopback(a) || a.Equals(IPAddress.Any) ? null : a;
        }

        /// <summary>
        /// Pure choice: on the interface that owns <paramref name="routeSource"/>, a global, non-temporary, preferred
        /// IPv6 address (same /64 first, then the longest preferred lifetime); otherwise the route source itself.
        /// </summary>
        public static IPAddress? PickStableIpv6(IPAddress? routeSource, IList<AddressCandidate> all)
        {
            if (routeSource == null || !IsGlobalUnicastV6(routeSource)) return null;
            string? iface = null;
            foreach (AddressCandidate c in all)
                if (c.Address.Equals(routeSource)) { iface = c.InterfaceId; break; }
            if (iface == null) return routeSource;

            AddressCandidate? best = null;
            foreach (AddressCandidate c in all)
            {
                if (c.InterfaceId != iface || c.Temporary || !c.Preferred || !IsGlobalUnicastV6(c.Address)) continue;
                if (best == null) { best = c; continue; }
                bool cSame = SamePrefix64(c.Address, routeSource), bSame = SamePrefix64(best.Address, routeSource);
                if (cSame != bSame) { if (cSame) best = c; continue; }
                if (c.PreferredLifetimeSeconds > best.PreferredLifetimeSeconds) best = c;
            }
            return best?.Address ?? routeSource;
        }

        /// <summary>2000::/3 minus Teredo (2001::/32), 6to4 (2002::/16) and documentation (2001:db8::/32).</summary>
        public static bool IsGlobalUnicastV6(IPAddress a)
        {
            if (a.AddressFamily != AddressFamily.InterNetworkV6 || a.IsIPv4MappedToIPv6) return false;
            byte[] b = a.GetAddressBytes();
            if ((b[0] & 0xE0) != 0x20) return false;
            if (b[0] == 0x20 && b[1] == 0x01 && b[2] == 0x00 && b[3] == 0x00) return false;    // Teredo
            if (b[0] == 0x20 && b[1] == 0x02) return false;                                     // 6to4
            if (b[0] == 0x20 && b[1] == 0x01 && b[2] == 0x0d && b[3] == 0xb8) return false;    // documentation
            return true;
        }

        static bool SamePrefix64(IPAddress x, IPAddress y)
        {
            byte[] a = x.GetAddressBytes(), b = y.GetAddressBytes();
            if (a.Length != 16 || b.Length != 16) return false;
            for (int i = 0; i < 8; i++) if (a[i] != b[i]) return false;
            return true;
        }

        /// <summary>
        /// Every unicast address of every interface that is up. The Windows-only properties (suffix origin, DAD state,
        /// lifetimes) throw PlatformNotSupportedException elsewhere; then an address counts as stable and preferred.
        /// </summary>
        public static List<AddressCandidate> Read()
        {
            var list = new List<AddressCandidate>();
            NetworkInterface[] nics;
            try { nics = NetworkInterface.GetAllNetworkInterfaces(); }
            catch (Exception) { return list; }
            foreach (NetworkInterface nic in nics)
            {
                try
                {
                    if (nic.OperationalStatus != OperationalStatus.Up) continue;
                    foreach (UnicastIPAddressInformation u in nic.GetIPProperties().UnicastAddresses)
                    {
                        bool temporary = false, preferred = true;
                        long lifetime = long.MaxValue;
                        try { temporary = u.SuffixOrigin == SuffixOrigin.Random; } catch (Exception) { }
                        try { preferred = u.DuplicateAddressDetectionState == DuplicateAddressDetectionState.Preferred; } catch (Exception) { }
                        try { lifetime = u.AddressPreferredLifetime; } catch (Exception) { }
                        list.Add(new AddressCandidate(u.Address, nic.Id, temporary, preferred, lifetime));
                    }
                }
                catch (Exception)
                {
                    // an interface that disappears while we look at it
                }
            }
            return list;
        }
    }
}
