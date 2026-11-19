// udpmux spreads one UDP flow (WireGuard) over N UDP flows so that phantun
// opens N fake-TCP connections, and merges them back on the other side.
//
// client: WireGuard -> -listen -> round robin over N sockets -> -upstream (phantun_client --local)
// server: phantun_server --remote -> -listen -> one socket -> -upstream (WireGuard);
//
//	replies from WireGuard go round robin over the phantun flows seen in the last -idle.
//
// The client sends a 1-byte keepalive (0x00) on every socket each -keepalive so
// idle phantun connections stay open; the server drops it. WireGuard messages
// are never 1 byte.
//
// Packets are read and written in batches (recvmmsg/sendmmsg), up to -batch
// per system call, and socket buffers are forced to -buffer (SO_RCVBUFFORCE,
// needs CAP_NET_ADMIN; falls back to the rmem_max-capped size).
package main

import (
	"flag"
	"log"
	"net"
	"net/netip"
	"sync"
	"sync/atomic"
	"syscall"
	"time"

	"golang.org/x/net/ipv4"
	"golang.org/x/sys/unix"
)

const pktSize = 2048 // > any WireGuard packet on a 1500-byte path

func isKeepalive(b []byte) bool { return len(b) == 1 && b[0] == 0 }

var keepaliveMsg = []byte{0}

func setBuffers(c *net.UDPConn, size int) {
	raw, err := c.SyscallConn()
	if err == nil {
		forced := true
		_ = raw.Control(func(fd uintptr) {
			if unix.SetsockoptInt(int(fd), unix.SOL_SOCKET, unix.SO_RCVBUFFORCE, size) != nil ||
				unix.SetsockoptInt(int(fd), unix.SOL_SOCKET, unix.SO_SNDBUFFORCE, size) != nil {
				forced = false
			}
		})
		if forced {
			return
		}
	}
	_ = c.SetReadBuffer(size)
	_ = c.SetWriteBuffer(size)
}

// batch is a reusable set of messages with their own buffers.
type batch struct {
	msgs []ipv4.Message
}

func newBatch(n int) *batch {
	b := &batch{msgs: make([]ipv4.Message, n)}
	for i := range b.msgs {
		b.msgs[i].Buffers = [][]byte{make([]byte, pktSize)}
	}
	return b
}

// writeAll sends every message, retrying partial batches.
func writeAll(pc *ipv4.PacketConn, ms []ipv4.Message) {
	for len(ms) > 0 {
		n, err := pc.WriteBatch(ms, 0)
		if err != nil {
			if n <= 0 {
				// Drop the first message (e.g. ECONNREFUSED while the peer restarts) and go on.
				n = 1
			}
		}
		ms = ms[n:]
	}
}

func main() {
	mode := flag.String("mode", "", "client or server")
	listen := flag.String("listen", "", "UDP address to listen on")
	upstream := flag.String("upstream", "", "UDP address to send to")
	n := flag.Int("n", 8, "client: number of flows (phantun connections)")
	keepalive := flag.Duration("keepalive", 10*time.Second, "client: keepalive interval per flow")
	idle := flag.Duration("idle", 60*time.Second, "server: forget a flow after this idle time")
	buf := flag.Int("buffer", 4<<20, "socket buffer bytes")
	bsize := flag.Int("batch", 64, "packets per system call")
	flag.Parse()
	log.SetFlags(0)
	log.SetPrefix("udpmux: ")
	if *bsize < 1 {
		*bsize = 1
	}

	laddr, err := net.ResolveUDPAddr("udp", *listen)
	if err != nil {
		log.Fatalf("listen %q: %v", *listen, err)
	}
	uaddr, err := net.ResolveUDPAddr("udp", *upstream)
	if err != nil {
		log.Fatalf("upstream %q: %v", *upstream, err)
	}
	lconn, err := net.ListenUDP("udp", laddr)
	if err != nil {
		log.Fatal(err)
	}
	setBuffers(lconn, *buf)

	switch *mode {
	case "client":
		if *n < 1 {
			log.Fatal("-n must be >= 1")
		}
		runClient(lconn, uaddr, *n, *keepalive, *buf, *bsize)
	case "server":
		runServer(lconn, uaddr, *idle, *buf, *bsize)
	default:
		log.Fatal("-mode must be client or server")
	}
}

func runClient(lconn *net.UDPConn, uaddr *net.UDPAddr, n int, keepalive time.Duration, buf, bsize int) {
	lpc := ipv4.NewPacketConn(lconn)
	socks := make([]*net.UDPConn, n)
	pcs := make([]*ipv4.PacketConn, n)
	for i := range socks {
		c, err := net.DialUDP("udp", nil, uaddr)
		if err != nil {
			log.Fatal(err)
		}
		setBuffers(c, buf)
		socks[i] = c
		pcs[i] = ipv4.NewPacketConn(c)
	}
	log.Printf("client: %s -> %d flows -> %s (batch %d)", lconn.LocalAddr(), n, uaddr, bsize)

	var peer atomic.Pointer[net.UDPAddr] // the WireGuard endpoint that talks to us

	// phantun -> WireGuard, one reader per flow.
	for _, pc := range pcs {
		go func(pc *ipv4.PacketConn) {
			in := newBatch(bsize)
			out := make([]ipv4.Message, 0, bsize)
			for {
				k, err := pc.ReadBatch(in.msgs, 0)
				if err != nil {
					// ECONNREFUSED while phantun restarts: keep going.
					time.Sleep(100 * time.Millisecond)
					continue
				}
				p := peer.Load()
				if p == nil {
					continue
				}
				out = out[:0]
				for i := 0; i < k; i++ {
					m := &in.msgs[i]
					b := m.Buffers[0][:m.N]
					if isKeepalive(b) {
						continue
					}
					out = append(out, ipv4.Message{Buffers: [][]byte{b}, Addr: p})
				}
				writeAll(lpc, out)
			}
		}(pc)
	}
	go func() {
		t := time.NewTicker(keepalive)
		for range t.C {
			for _, c := range socks {
				_, _ = c.Write(keepaliveMsg)
			}
		}
	}()

	// WireGuard -> phantun flows. Each batch goes whole to one flow and the
	// next batch to the next flow, so packets keep their order: splitting a
	// batch packet by packet over the flows and writing flow after flow
	// reorders it, and the TCP inside WireGuard collapses (seen: 4 Mbit/s).
	in := newBatch(bsize)
	out := make([]ipv4.Message, 0, bsize)
	var cur netip.AddrPort
	rr := 0
	for {
		k, err := lpc.ReadBatch(in.msgs, 0)
		if err != nil {
			log.Printf("read: %v", err)
			time.Sleep(10 * time.Millisecond)
			continue
		}
		out = out[:0]
		for i := 0; i < k; i++ {
			m := &in.msgs[i]
			if ua, ok := m.Addr.(*net.UDPAddr); ok {
				if ap := ua.AddrPort(); ap != cur {
					cur = ap
					peer.Store(ua)
					log.Printf("client: peer %s", ua)
				}
			}
			out = append(out, ipv4.Message{Buffers: [][]byte{m.Buffers[0][:m.N]}})
		}
		writeAll(pcs[rr], out)
		rr++
		if rr == n {
			rr = 0
		}
	}
}

type flowEntry struct {
	addr *net.UDPAddr
	seen atomic.Int64 // unix nanos
}

func runServer(lconn *net.UDPConn, uaddr *net.UDPAddr, idle time.Duration, buf, bsize int) {
	lpc := ipv4.NewPacketConn(lconn)
	wconn, err := net.DialUDP("udp", nil, uaddr)
	if err != nil {
		log.Fatal(err)
	}
	setBuffers(wconn, buf)
	wpc := ipv4.NewPacketConn(wconn)
	log.Printf("server: %s -> %s (single flow, batch %d)", lconn.LocalAddr(), uaddr, bsize)

	var mu sync.Mutex
	flows := map[netip.AddrPort]*flowEntry{}
	var active atomic.Pointer[[]*flowEntry] // snapshot for the reply path
	empty := []*flowEntry{}
	active.Store(&empty)
	rebuild := func(now int64) { // mu held
		list := make([]*flowEntry, 0, len(flows))
		for k, f := range flows {
			if now-f.seen.Load() > int64(idle) {
				delete(flows, k)
				continue
			}
			list = append(list, f)
		}
		active.Store(&list)
	}
	go func() {
		t := time.NewTicker(5 * time.Second)
		for range t.C {
			mu.Lock()
			before := len(flows)
			rebuild(time.Now().UnixNano())
			if after := len(flows); after != before {
				log.Printf("server: %d flows", after)
			}
			mu.Unlock()
		}
	}()

	// WireGuard -> phantun flows, round robin per packet.
	go func() {
		in := newBatch(bsize)
		out := make([]ipv4.Message, 0, bsize)
		rr := 0
		for {
			k, err := wpc.ReadBatch(in.msgs, 0)
			if err != nil {
				time.Sleep(100 * time.Millisecond)
				continue
			}
			list := *active.Load()
			if len(list) == 0 {
				continue
			}
			out = out[:0]
			for i := 0; i < k; i++ {
				m := &in.msgs[i]
				if rr >= len(list) {
					rr = 0
				}
				out = append(out, ipv4.Message{Buffers: [][]byte{m.Buffers[0][:m.N]}, Addr: list[rr].addr})
				rr++
			}
			writeAll(lpc, out)
		}
	}()

	// phantun flows -> WireGuard.
	in := newBatch(bsize)
	out := make([]ipv4.Message, 0, bsize)
	for {
		k, err := lpc.ReadBatch(in.msgs, 0)
		if err != nil {
			if err != syscall.EINTR {
				log.Printf("read: %v", err)
			}
			time.Sleep(10 * time.Millisecond)
			continue
		}
		now := time.Now().UnixNano()
		out = out[:0]
		var newFlows []string
		mu.Lock()
		for i := 0; i < k; i++ {
			m := &in.msgs[i]
			ua, ok := m.Addr.(*net.UDPAddr)
			if !ok {
				continue
			}
			ap := ua.AddrPort()
			f, seen := flows[ap]
			if !seen {
				f = &flowEntry{addr: ua}
				flows[ap] = f
				newFlows = append(newFlows, ap.String())
			}
			f.seen.Store(now)
			if b := m.Buffers[0][:m.N]; !isKeepalive(b) {
				out = append(out, ipv4.Message{Buffers: [][]byte{b}})
			}
		}
		if len(newFlows) > 0 {
			rebuild(now)
		}
		total := len(flows)
		mu.Unlock()
		for _, s := range newFlows {
			log.Printf("server: new flow %s (%d flows)", s, total)
		}
		writeAll(wpc, out)
	}
}
