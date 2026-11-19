# گزارش تونل Phantun روی میکروتیک (trafx-phantun)

تاریخ: ۲۰۲۶-۱۱-۱۸ / ۱۹ (UTC)، نسخه‌ی **0.3.1**
وضعیت: **تستی**. روی دو روتر تستی کار می‌کنه و هیچ کاربر واقعی از این تونل رد نمی‌شه.

**خلاصه:** بین دو میکروتیک با ۸۲ میلی‌ثانیه تأخیر، با ۴ اتصال و در حالت ریورس، سرعت **۱۳۴ مگابیت ارسال و ۱۳۵ مگابیت دریافت** بود. نسخه‌ی اول فقط ۳.۴ مگابیت می‌داد.

---

## ۱. چرا سراغ phantun رفتیم

- روی یک سرور ایران، یک تونل تجاری (TLS معکوس + TPROXY) برای حدود ۲۰۰ کاربر ocserv حدود **۱.۲ هسته CPU و ۷۴۴ مگ رم** مصرف می‌کرد. یعنی بیشتر از کل ocserv، که برای همین کاربرها ۱.۳ هسته و ۱۷۸ مگ مصرف داره.
- تست‌های قبلی نشون داد **UDP و GRE از ایران به خارج محدود می‌شن ولی TCP آزاده**. مثلاً به NL: TCP مستقیم ۸۵ مگابیت، GRE ۱۴ و L2TP/UDP ۳.۸ مگابیت.
- پس دنبال تونلی بودیم که **روی شبکه TCP دیده بشه** ولی سبک باشه.

## ۲. phantun چیه

- پروژه‌ی متن‌باز [dndx/phantun](https://github.com/dndx/phantun) با لایسنس Apache-2.0، به زبان Rust. نسخه‌ی مورد استفاده v0.8.1.
- **UDP رو به شکل TCP جعلی می‌فرسته.** یک handshake واقعی TCP انجام می‌ده (SYN، SYN-ACK، ACK) و بعد هر بسته‌ی UDP رو با یک هدر TCP می‌فرسته.
- بسته‌ی گم‌شده رو دوباره نمی‌فرسته و کنترل ازدحام نداره. پس مشکل «TCP داخل TCP» پیش نمیاد.
- **خودش رمزنگاری نداره.** رمزنگاری کار WireGuard است که روی RouterOS اجرا می‌شه.
- سربار هر بسته فقط ۱۲ بایته. MTU تونل WireGuard باید **1428** باشه.

```
کاربرها ─► WireGuard روی RouterOS (wg-phantun) ─UDP─► کانتینر trafx-phantun (client)
    ══ TCP جعلی به SERVER:4567 ══►
کانتینر trafx-phantun (server) ─UDP─► WireGuard روی روتر خارج ─► اینترنت
```

## ۳. image کانتینر: trafx-phantun

مسیر روی سرور dev: `/opt/trafx-phantun`

| فایل | کار |
|---|---|
| `Dockerfile` | alpine 3.20 + iptables، باینری‌های آماده‌ی phantun 0.8.1 و udpmux |
| `entrypoint.sh` | تنظیم کامل از طریق env، ساخت قانون NAT و پاک کردنش موقع توقف |
| `udpmux/main.go` | پخش یک جریان WireGuard روی چند اتصال TCP جعلی (ماژول‌های Go داخل `udpmux/vendor` هستن) |
| `scripts/fetch-phantun.sh` | دانلود باینری‌های phantun و بررسی چک‌سام اون‌ها با `bin/SHA256SUMS` |
| `scripts/image.sh` | ساخت فایل tar در قالب docker-archive کلاسیک (RouterOS قالب جدید `docker save` رو نمی‌پذیره) |
| `dist/trafx-phantun-{amd64,arm64,armv7}.tar` | خروجی آماده‌ی نصب. توی git نیست |

ساخت image:
```
scripts/fetch-phantun.sh
scripts/image.sh 0.3.1 amd64
```

### env‌ها

| متغیر | سمت | پیش‌فرض | توضیح |
|---|---|---|---|
| `PHANTUN_MODE` | هر دو | — | `client` یا `server` |
| `PHANTUN_REMOTE` | client | — | آدرس سرور به شکل `IP:4567` |
| `PHANTUN_LISTEN` | client | `0.0.0.0:51830` | آدرسی که WireGuard روتر به‌عنوان endpoint استفاده می‌کنه |
| `PHANTUN_CONNECTIONS` | client | `1` | تعداد اتصال‌های TCP جعلی. روی CHR دو هسته‌ای **۴** بهترین نتیجه رو داد |
| `PHANTUN_CONNECTIONS` | server | خالی | فقط اگه همه‌ی کلاینت‌ها ۱ اتصال دارن، با `1` udpmux سمت سرور خاموش می‌شه. در حالت پیش‌فرض روشنه و هر تعداد اتصالی رو قبول می‌کنه |
| `PHANTUN_PORT` | server | `4567` | پورت TCP |
| `PHANTUN_FORWARD` | server | — | آدرس WireGuard روتر، مثلاً `172.31.252.1:13240` |
| `PHANTUN_BUFFER` | هر دو | `4194304` | اندازه‌ی بافر سوکت‌های udpmux |
| `PHANTUN_HANDSHAKE_PACKET` | هر دو | — | فایلی که بعد از handshake به‌عنوان اولین بسته فرستاده می‌شه |

## ۴. دو یافته‌ی مهم

### الف) conntrack داخل کانتینر سرعت رو کم می‌کرد (نسخه‌ی 0.1.1 حلش کرد)
phantun موقع handshake امکان window scaling رو اعلام نمی‌کنه. پس conntrack فرض می‌کنه پنجره‌ی TCP حداکثر ۶۴ کیلوبایته. با بیشتر شدن داده‌ی در راه، بسته‌ها `INVALID` حساب می‌شن و **از NAT رد نمی‌شن**، پس گم می‌شن.
- **راه‌حل:** entrypoint موقع شروع `nf_conntrack_tcp_be_liberal=1` رو فعال می‌کنه. این تنظیم فقط برای network namespace خود کانتینره و روی روتر اثری نداره.
- **نتیجه روی Docker با تأخیر ۸۵ میلی‌ثانیه:** سرعت از **۱۴.۵ به ۲۱۳ مگابیت** رسید، تقریباً هم‌اندازه‌ی WireGuard مستقیم (۱۹۳).
- **نتیجه روی میکروتیک:** ارسال از **۳.۴ به ۷۰ مگابیت** رسید.
- روی Docker، `/proc/sys` فقط‌خواندنیه. باید با `--sysctl net.netfilter.nf_conntrack_tcp_be_liberal=1` اجرا بشه و نه با `--network host`.
- **روی خود روترها** (که `liberal-tcp-tracking=no` دارن) هیچ تغییری لازم نشد. شمارنده‌ی invalid حین تست روی روتر تستی ۰ و روی روتر خارج ۱ بود.

### ب) چند اتصال موازی (نسخه‌ی 0.2.0، udpmux)
- **سمت کلاینت:** udpmux بسته‌های WireGuard رو به‌نوبت روی N سوکت UDP می‌فرسته و phantun برای هر سوکت یک اتصال TCP جعلی جدا می‌سازه.
- **سمت سرور:** udpmux همیشه فعاله. این N اتصال رو دوباره یکی می‌کنه و به WireGuard می‌ده، و جواب‌ها رو به‌نوبت از همون اتصال‌ها برمی‌گردونه.
- هر ۱۰ ثانیه یک بسته‌ی ۱ بایتی keepalive روی هر اتصال فرستاده می‌شه تا اتصال‌های بی‌کار بسته نشن. اتصالی که ۶۰ ثانیه بی‌کار باشه حذف می‌شه.
- **هزینه:** هر بسته یک بار دیگه توی userspace جابه‌جا می‌شه، پس CPU بیشتری مصرف می‌شه.

### ج) بهینه‌سازی udpmux (نسخه‌ی 0.3.x)
- **سقف نسخه‌ی 0.2.0:** هر بسته با یک syscall جدا خونده و فرستاده می‌شد. نتیجه حدود ۶۰ مگابیت با loss زیاد و نصف یک هسته CPU برای هر طرف بود.
- **نسخه‌ی 0.3.0:**
  - خوندن و فرستادن دسته‌ای با `recvmmsg` و `sendmmsg`، تا ۶۴ بسته در هر syscall
  - بافر سوکت‌ها به‌اجبار ۴ مگابایت (`SO_RCVBUFFORCE`)
  - روی Docker حدود ۳۰ تا ۴۰٪ CPU کمتر به ازای هر مگابیت و loss خیلی کمتر
- **باگ 0.3.0 و رفعش در 0.3.1:** کلاینت هر دسته رو بسته به بسته بین سوکت‌ها پخش می‌کرد و بعد سوکت‌ها رو یکی‌یکی می‌فرستاد. این کار ترتیب بسته‌ها رو به هم می‌ریخت و TCP داخل تونل سقوط می‌کرد: ارسال ۴ مگابیت شد. در 0.3.1 **کل هر دسته روی یک اتصال** می‌ره و دسته‌ی بعدی روی اتصال بعدی، پس ترتیب حفظ می‌شه.

## ۵. نتایج تست

### روی لینوکس، سرور ایران به سرور آمریکا، نسخه‌ی بدون رفع conntrack
| مسیر | ارسال | دریافت |
|---|---|---|
| TCP مستقیم | ۳.۳ مگابیت | — |
| phantun + WireGuard | ۱۷.۳ مگابیت | ۵.۵ مگابیت |

این تست بعداً کامل پاک شد.

### تونل تجاری TLS، ایران به هلند (همزمان با کاربرهای واقعی)
| ارسال | دریافت |
|---|---|
| ۱۰.۸ مگابیت | ۳۵.۶ مگابیت |

### میکروتیک به میکروتیک: CHR تستی ایران به میکروتیک خارج
همه‌ی تست‌ها با btest خود RouterOS، با ۲۰ اتصال TCP و تأخیر ۸۲ میلی‌ثانیه گرفته شدن.

| نسخه / تنظیم | ارسال | دریافت | CPU دو روتر |
|---|---|---|---|
| TCP مستقیم، بدون تونل | ۴۰۰ مگابیت (با ۱ اتصال: ۲۸۰) | — | ۲۰ / ۳۰٪ |
| 0.1.0 | ۳.۴ مگابیت | ۲۲ مگابیت | ۳ / ۵٪ |
| 0.1.1 (رفع conntrack) | ۷۰ مگابیت، ۶۰ ثانیه ثابت | ۵۳ مگابیت | ۲۰ / ۳۲٪ |
| 0.2.0 با ۸ اتصال | ۷۵ مگابیت (لحظه‌ای تا ۱۱۱) | ۸۱ مگابیت | ۶۸ / ۷۰٪ |
| 0.2.0 با ۴ اتصال | — | ۸۳ تا ۱۰۱ مگابیت | حدود ۶۰٪ |
| 0.3.0 با ۴ اتصال (باگ ترتیب بسته‌ها) | ۴ مگابیت | ۱۲۸ مگابیت | ۱۰ / ۶۹٪ |
| 0.3.1 با ۴ اتصال | ۱۱۸ مگابیت | ۱۲۸ مگابیت | ۵۳ تا ۷۴ / ۶۳٪ |
| **0.3.1 با ۴ اتصال، ریورس** | **۱۳۴ مگابیت** | **۱۳۵ مگابیت** | ۵۶ تا ۶۴ / **۸۷٪** |

**ریورس** یعنی روتر ایران سرور phantun است و روی پورت 4567 گوش می‌ده، و روتر خارج به اون وصل می‌شه. تنظیم WireGuard عوض نمی‌شه. در ریورس، کار پخش بسته‌ها (udpmux کلاینت) روی روتر خارج انجام می‌شه، پس روتر خارج CPU بیشتری لازم داره.

**تست جانبی، REALITY:** روش WireGuard داخل VLESS + REALITY (با Xray) هم روی Docker تست شد. فقط ۶ تا ۱۳ مگابیت داد، چون Xray بسته‌های UDP رو کند می‌خونه و بسته‌ها دور ریخته می‌شن. فعلاً کنار گذاشته شده.

- **MTU** علت مشکل نبود: با MTU 1280 هم همون سرعت گرفته شد.
- **پایداری:** بعد از ری‌استارت کلاینت یا سرور، تونل در عرض چند ثانیه خودش دوباره وصل می‌شه.
- **چرا به ۲۰۰ مگابیتِ Docker نمی‌رسه:** گلوگاه **CPU** روی CHR دو هسته‌ایه. RouterOS، bridge، veth، phantun، udpmux، WireGuard و خود btest همه روی همون دو هسته اجرا می‌شن. مسیر اینترنت واقعی هم بی‌تأثیر نیست.

## ۶. نصب روی میکروتیک

پیش‌نیازها:
- پکیج container نصب باشه.
- `device-mode container=yes` فعال باشه. این کار به دسترسی فیزیکی به روتر نیاز داره.
- فایل `trafx-phantun-<arch>.tar` روی روتر آپلود شده باشه.

همه‌ی چیزهایی که ساخته می‌شن برچسب `trafx-phantun` دارن.

### سمت سرور (روتر خارج)
```
/interface bridge add name=br-trafx-phantun comment=trafx-phantun
/ip address add address=172.31.252.1/24 interface=br-trafx-phantun comment=trafx-phantun
/interface veth add name=veth-trafx-phantun address=172.31.252.2/24 gateway=172.31.252.1 comment=trafx-phantun
/interface bridge port add bridge=br-trafx-phantun interface=veth-trafx-phantun comment=trafx-phantun
/ip firewall nat add chain=srcnat action=masquerade src-address=172.31.252.0/24 comment=trafx-phantun

/interface wireguard add name=wg-phantun mtu=1428 listen-port=13240 comment=trafx-phantun
/ip address add address=10.250.0.2/30 interface=wg-phantun comment=trafx-phantun
/interface wireguard peers add interface=wg-phantun public-key="<کلید عمومی wg-phantun روتر کلاینت>" allowed-address=10.250.0.1/32 comment=trafx-phantun
/ip firewall nat add chain=srcnat action=masquerade src-address=10.250.0.0/30 comment=trafx-phantun

/container envs add list=ENV_TRAFX_PHANTUN key=PHANTUN_MODE value=server
/container envs add list=ENV_TRAFX_PHANTUN key=PHANTUN_FORWARD value=172.31.252.1:13240
/container add file=trafx-phantun-amd64.tar interface=veth-trafx-phantun envlists=ENV_TRAFX_PHANTUN root-dir=trafx-phantun-root name=trafx-phantun start-on-boot=yes restart-policy=on-failure logging=yes
# صبر کن تا وضعیت کانتینر stopped بشه، بعد:
/container start trafx-phantun

/ip firewall nat add chain=dstnat action=dst-nat protocol=tcp dst-address=<IP عمومی روتر> dst-port=4567 to-addresses=172.31.252.2 to-ports=4567 comment=trafx-phantun
```

### سمت کلاینت (روتر ایران)
```
# bridge، veth و masquerade مثل سمت سرور، با همون آدرس‌های 172.31.252.x
/interface wireguard add name=wg-phantun mtu=1428 listen-port=13240 comment=trafx-phantun
/ip address add address=10.250.0.1/30 interface=wg-phantun comment=trafx-phantun
/interface wireguard peers add interface=wg-phantun public-key="<کلید عمومی wg-phantun روتر سرور>" endpoint-address=172.31.252.2 endpoint-port=51830 allowed-address=10.250.0.2/32 persistent-keepalive=25s comment=trafx-phantun

/container envs add list=ENV_TRAFX_PHANTUN key=PHANTUN_MODE value=client
/container envs add list=ENV_TRAFX_PHANTUN key=PHANTUN_REMOTE value=<IP سرور>:4567
/container envs add list=ENV_TRAFX_PHANTUN key=PHANTUN_CONNECTIONS value=4
/container add file=trafx-phantun-amd64.tar interface=veth-trafx-phantun envlists=ENV_TRAFX_PHANTUN root-dir=trafx-phantun-root name=trafx-phantun start-on-boot=yes restart-policy=on-failure logging=yes
# صبر کن تا وضعیت کانتینر stopped بشه، بعد:
/container start trafx-phantun
```

### بررسی
```
/log print where message~"liberal|udpmux|flows"
/ping 10.250.0.2 count=5
```

در لاگ باید این خط‌ها دیده بشن:
- `conntrack tcp_be_liberal: 1`
- سمت کلاینت: `udpmux: client: ... -> 4 flows`
- سمت سرور: `(4 flows)`

### فرستادن ترافیک کاربرها از تونل
**این بخش هنوز روی هیچ روتری انجام نشده.** کاربرهای OVPN، L2TP، SSTP یا کانتینر ocserv رو می‌شه از تونل فرستاد:
```
/ip firewall nat add chain=srcnat action=masquerade out-interface=wg-phantun comment=trafx-phantun
/ip firewall mangle add chain=forward protocol=tcp tcp-flags=syn out-interface=wg-phantun action=change-mss new-mss=clamp-to-pmtu comment=trafx-phantun
/routing table add name=to-phantun fib
/ip route add dst-address=0.0.0.0/0 gateway=wg-phantun routing-table=to-phantun
/routing rule add src-address=<pool کاربرها> action=lookup table=to-phantun
```

- برای کانتینر TrafX Access Server (ocserv)، توی تنظیم لوکیشن‌ها خروجی رو `wg-phantun` بذار.
- ⚠️ subnet خود کانتینر phantun (`172.31.252.0/24`) **هیچ‌وقت** نباید به `to-phantun` فرستاده بشه، وگرنه تونل توی خودش حلقه می‌زنه.

### به‌روزرسانی کانتینر
```
/container stop trafx-phantun
# صبر کن تا وضعیت stopped بشه
/container remove trafx-phantun
# اینو تکرار کن تا هیچ خروجی‌ای نده
/container print where name=trafx-phantun
/file remove trafx-phantun-amd64.tar
# فایل جدید رو آپلود کن، بعد همون دستورهای container add و start بالا
```
اگه `container add` خطای `root-dir overlap` داد، یعنی کانتینر قبلی هنوز کامل پاک نشده.

## ۷. محدودیت‌ها و ریسک‌ها

- **phantun ظاهر TLS نداره.** DPI دقیق ممکنه تشخیص بده که این TCP واقعی نیست، چون retransmit نداره و payload‌ش شبیه TLS نیست. روی مسیرهایی که تست کردیم، محدود نشد.
- **همه‌چیز توی userspace اجرا می‌شه.** روی روترهای ضعیف، CPU سقف سرعت رو تعیین می‌کنه. با اتصال بیشتر، CPU بیشتری مصرف می‌شه.
- **روی ARM** فقط اجرای باینری زیر qemu بررسی شده. روی روتر ARM واقعی تست نشده.
- **تست با btest روی خود روتر** عدد واقعی تونل رو کمتر نشون می‌ده، چون btest هم CPU همون روتر رو مصرف می‌کنه.

## ۸. وضعیت فعلی و کارهای مونده

**الان روی روترها (حالت ریورس):**
- CHR تستی ایران **سرور** هست، با dst-nat پورت 4567 به کانتینر و نسخه‌ی 0.3.1.
- روتر خارج **کلاینت** هست، با نسخه‌ی 0.3.1 و ۴ اتصال. dst-nat پورت 4567 روی این روتر disable شده.
- هیچ ترافیک کاربری از تونل رد نمی‌شه.

**کارهای مونده:**
- [ ] `bandwidth-server` روی روتر خارج به حالت قبل برگرده (`authenticate=yes`).
- [ ] تست واقعی: سیستمی پشت روتر کاربرها ترافیکش رو از تونل رد کنه، به‌جای btest روی خود روتر.
- [ ] تست روی روتر ARM واقعی.
- [ ] تصمیم: حالت عادی یا ریورس برای روترهای واقعی، و CPU لازم برای روتر خارج.
- [ ] مسیر پشتیبان برای وقتی که phantun شناسایی بشه: wstunnel (WireGuard داخل WSS/TLS، با امکان کار پشت CDN) یا REALITY بهینه‌شده.
