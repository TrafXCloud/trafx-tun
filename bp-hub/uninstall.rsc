# Removes BackPack Hub and everything install.rsc made (comment bp-hub*). Tunnels made from the panel carry
# comment bp-<tunnel>; delete them in the panel first if their router rules should go too.
#   /tool fetch url="https://raw.githubusercontent.com/TrafXCloud/trafx-tun/main/bp-hub/uninstall.rsc"; /import uninstall.rsc
:put "bp-hub: removing"
:do {/container stop [find where name="bp-hub"]; :delay 5s} on-error={}
:do {/container remove [find where name="bp-hub"]} on-error={}
:do {/container envs remove [find where list="bp-hub"]} on-error={}
:do {/container mounts remove [find where list="bp-hub"]} on-error={}
:do {/ip firewall filter remove [find where comment="bp-hub-api"]} on-error={}
:do {/ip firewall nat remove [find where comment~"^bp-hub"]} on-error={}
:do {/user remove [find where name="bp-api"]} on-error={}
:do {/user group remove [find where name="bp-api"]} on-error={}
:do {/ip address remove [find where comment="bp-hub"]} on-error={}
:do {/interface bridge port remove [find where comment="bp-hub"]} on-error={}
:do {/interface veth remove [find where comment="bp-hub"]} on-error={}
:do {/interface bridge remove [find where comment="bp-hub"]} on-error={}
:put "bp-hub: removed. The www service was left as it is; tunnel config stays in /bp-hub/cfg (delete that folder in Files if not needed)."
