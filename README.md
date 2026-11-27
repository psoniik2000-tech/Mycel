# Mycel

Мессенджер без центральных серверов, устойчивый к блокировкам. Для закрытой группы до 50 человек. Работает на ПК (Windows, Linux, macOS) и телефоне (Android).

- P2P-сеть на QUIC (iroh), запасные транспорты: TLS на 443, Tor, локальная сеть, офлайн-пакеты
- Сквозное шифрование (MLS), зашифрованное локальное хранилище
- Текст, фото, видео, файлы, голосовые, видеокружки, позже — звонки
- Ядро на Rust, интерфейс на Tauri 2

Статус: работает на ПК (текст, медиа, звонки); Android в разработке. Текущее состояние и задачи — [docs/STATUS.md](docs/STATUS.md), описание — [docs/DESIGN.md](docs/DESIGN.md).

Эмодзи — [Twemoji](https://github.com/jdecked/twemoji) (© Twitter, Inc и участники), графика под [CC-BY 4.0](https://creativecommons.org/licenses/by/4.0/).

## Якорь (ПК или сервер, включённый 24/7)

Якорь — обычный участник, который хранит сообщения для тех, кто офлайн (30 дней), и держит relay для тех, кто за NAT.

**Проще всего — в приложении:** на ПК включите режим пространств («Настройки» → «Другие способы»), затем «Настройки» → «Связь» → «Сделать этот ПК якорем». Порт на роутере откроется сам (UPnP), отдельная программа не нужна. Работает, пока Mycel запущен (можно свернуть в трей).

**Без экрана (VPS, сервер, ПК без входа в систему)** — программа `mycel-anchor`:

1. Скачать: GitHub → Actions → последний успешный запуск CI на `main` → Artifacts → `mycel-anchor-linux-x64` или `mycel-anchor-windows-x64`. Или собрать: `cargo build --release -p mycel-anchor`.
2. Открыть TCP-порт (например 3340): на VPS — в файрволе хостера; дома — проброс на роутере. Узнать внешний IP (например, на 2ip.ru).
3. Вступить в пространство по инвайту из приложения (пока своего relay нет, через публичные n0):
   `mycel-anchor --n0-relay --dir ./mycel-data join mycel://join/...` (спросит пароль якоря; после «joined» — Ctrl+C).
4. Запустить: `mycel-anchor --dir ./mycel-data serve http://ВНЕШНИЙ-IP:3340`. Если пишет `WARNING: cannot reach own relay` — порт снаружи закрыт.
5. Адрес `http://ВНЕШНИЙ-IP:3340` можно добавить участникам в «Настройки» → «Связь» → «Резервные серверы»; новые приглашения получат его сами.

Автозапуск на Linux (systemd), от root:

```sh
install -m 755 mycel-anchor /usr/local/bin/
mkdir -p /var/lib/mycel && cd /var/lib/mycel
MYCEL_PASSWORD=ПАРОЛЬ mycel-anchor --n0-relay --dir . join 'mycel://join/...'
cat > /etc/systemd/system/mycel-anchor.service <<EOF
[Unit]
After=network-online.target
[Service]
Environment=MYCEL_PASSWORD=ПАРОЛЬ
ExecStart=/usr/local/bin/mycel-anchor --dir /var/lib/mycel serve http://ВНЕШНИЙ-IP:3340
Restart=always
[Install]
WantedBy=multi-user.target
EOF
systemctl enable --now mycel-anchor
```

Автозапуск на Windows (Планировщик заданий, при включении ПК, без входа в систему), в PowerShell от администратора:

```powershell
$a = New-ScheduledTaskAction -Execute "cmd.exe" -Argument '/c set MYCEL_PASSWORD=ПАРОЛЬ&& C:\MycelAnchor\mycel-anchor.exe --dir C:\MycelAnchor serve http://ВНЕШНИЙ-IP:3340'
Register-ScheduledTask MycelAnchor -Action $a -Trigger (New-ScheduledTaskTrigger -AtStartup) -User SYSTEM -RunLevel Highest
New-NetFirewallRule -DisplayName MycelAnchor -Direction Inbound -Protocol TCP -LocalPort 3340 -Action Allow
```

Пароль хранится открытым текстом (в задании или в unit-файле) — используйте отдельный пароль только для якоря. Остановить: `Stop-ScheduledTask MycelAnchor` / `systemctl stop mycel-anchor`.
