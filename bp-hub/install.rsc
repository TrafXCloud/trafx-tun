# BackPack Hub installer for RouterOS 7.20+ on x86_64. The same on the Iran and the abroad router;
# which side a router is gets chosen later in the panel. In the router terminal:
#   /tool fetch url="https://raw.githubusercontent.com/TrafXCloud/trafx-tun/main/bp-hub/install.rsc"; /import install.rsc
# It downloads the image, finds a free /30 and a free panel port, makes a random panel password,
# starts the container and prints the panel address and password at the end.
# Everything it makes carries comment bp-hub* (uninstall.rsc removes exactly that).

:local base "https://raw.githubusercontent.com/TrafXCloud/trafx-tun/main/bp-hub"
:local img "bp-hub.tar"

:put "bp-hub: checking the router"
:if ([/system resource get architecture-name] != "x86_64") do={:error "bp-hub needs an x86_64 router (CHR or x86)."}
:if ([:len [/system package find where name="container"]] = 0) do={:error "Install the container package first (System > Packages), then run this again."}
:if ([/system device-mode get container] != true) do={
  :put "Container mode is off. Run:  /system/device-mode/update container=yes"
  :put "then press the router's button or power-cycle it, and run this installer again."
  :error "container mode off"}
:if ([:len [/container find where name="bp-hub"]] > 0) do={:error "bp-hub is already installed. To reinstall, run uninstall.rsc first."}

# a free /30 for router <-> container: 172.30.90.0/30, else the next free 172.30.x.0/30
:local net ""
:for i from=90 to=130 do={
  :if ($net = "") do={
    :local c ("172.30." . $i)
    :if ([:len [/ip address find where address~("^172\\.30\\." . $i . "\\.")]] = 0) do={:set net $c}}}
:if ($net = "") do={:error "No free 172.30.90-130.0/30 range on this router."}

# a free port for the panel on the router: 9090, else the next one not used by a dst-nat or a service
:local port 0
:for p from=9090 to=9190 do={
  :if ($port = 0) do={
    :if ([:len [/ip firewall nat find where chain=dstnat dst-port=[:tostr $p]]] = 0 && [:len [/ip service find where port=$p]] = 0) do={:set port $p}}}
:if ($port = 0) do={:error "No free panel port in 9090-9190."}

:local pass [:rndstr length=12 from="abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789"]
:local key [:rndstr length=24 from="abcdefghjkmnpqrstuvwxyz23456789"]

:put ("bp-hub: downloading the image (about 23 MB) from " . $base)
:do {/tool fetch url=($base . "/" . $img) dst-path=$img} on-error={:error ("Download failed. Check that this router can open " . $base . " (DNS, internet, filtering).")}
:if ([/file get [find where name=$img] size] < 1000000) do={:error "The downloaded image is too small; the download did not complete."}

:put ("bp-hub: network " . $net . ".0/30, panel port " . $port)
/interface bridge add name=br-bp-hub comment=bp-hub
/interface veth add name=veth-bp-hub address=($net . ".2/30") gateway=($net . ".1") comment=bp-hub
/interface bridge port add bridge=br-bp-hub interface=veth-bp-hub comment=bp-hub
/ip address add address=($net . ".1/30") interface=br-bp-hub comment=bp-hub
/ip firewall nat add chain=srcnat src-address=($net . ".2") action=masquerade comment=bp-hub
/ip firewall nat add chain=dstnat dst-address-type=local protocol=tcp dst-port=[:tostr $port] action=dst-nat to-addresses=($net . ".2") to-ports=8080 comment=bp-hub-web
# the panel runs router commands itself through this user: only from the container's address
/user group add name=bp-api policy=read,write,api,rest-api,test comment=bp-hub
/user add name=bp-api group=bp-api password=$key address=($net . ".2/32") comment=bp-hub
# the router's REST API (www service) must answer the container; keep whatever was allowed before
:local was [/ip service get [find where name="www"] available-from]
:local from ($net . ".2/32")
:if ([:len $was] > 0) do={:set from ($was . "," . $net . ".2/32")}
/ip service set [find where name="www"] disabled=no available-from=$from
:if ([:len [/ip firewall filter find]] > 0) do={
  /ip firewall filter add chain=input src-address=($net . ".2") protocol=tcp dst-port=80 action=accept comment=bp-hub-api place-before=([/ip firewall filter find]->0)
} else={
  /ip firewall filter add chain=input src-address=($net . ".2") protocol=tcp dst-port=80 action=accept comment=bp-hub-api}
/container envs add list=bp-hub key=PANEL_PASS value=$pass
/container envs add list=bp-hub key=ROS_PASS value=$key
/container mounts add list=bp-hub src=/bp-hub/cfg dst=/etc/backpack comment=bp-hub
/container add file=$img name=bp-hub interface=veth-bp-hub root-dir=/bp-hub/root mountlists=bp-hub envlists=bp-hub start-on-boot=yes logging=yes comment=bp-hub

:put "bp-hub: unpacking the image and starting (up to 5 minutes)"
:local started false
:for i from=1 to=60 do={
  :if (!$started) do={
    :delay 5s
    :do {/container start [find where name="bp-hub"]; :set started true} on-error={}}}
:do {/file remove [find where name=$img]} on-error={}

# the router's public address: the one on the interface of the default route
:local pub "<router public IP>"
:do {
  :local gw [/ip route get [find where dst-address=0.0.0.0/0 active routing-table=main] immediate-gw]
  :local ifc [:pick $gw ([:find $gw "%"] + 1) [:len $gw]]
  :local a [/ip address get [find where interface=$ifc] address]
  :set pub [:pick $a 0 [:find $a "/"]]
} on-error={}

:put ""
:if ($started) do={:put "BackPack Hub is installed and running."} else={:put "BackPack Hub is installed, but it did not start yet. Check /container print, then: /container start bp-hub"}
:put ("  Panel:    http://" . $pub . ":" . $port)
:put  "  User:     admin"
:put ("  Password: " . $pass)
:put  "Keep the password. It is also in /container envs (key PANEL_PASS); change it there and restart the container."
:put  "Next: open the panel. On the Iran router make the tunnel first; on the abroad router paste its connection code."
