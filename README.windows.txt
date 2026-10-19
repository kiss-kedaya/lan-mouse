lan-mouse（Windows 版）
=======================
直接双击 lan-mouse.exe。不要单独拷走 exe：同目录的 DLL 是运行依赖。

lan-mouse-cli.exe  命令行：code / devices / pair / list
daemon.exe         无界面的后台服务

配对：另一台设备（Mac / Linux）界面里显示配对码，本机在它上面
选择发现的设备并输入配对码；或让对端先连过来，本机弹窗确认。
界面语言跟随系统，也可用 --language zh_CN。
端口默认 4242，被占用时用 --port 换一个。
