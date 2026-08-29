;; SCSP debug socket 客户端（docs/debug-diagnostics-logging.md）。
;;
;; 协议：Unix domain socket + 4 字节 big-endian 长度前缀 + JSON-RPC 2.0 body。
;;
;; 一次性调用（bb debug 任务使用）：
;;   (call "/path/to/d.sock" "runtime.plugins" {})
;;
;; REPL 工作流（推荐，交互式调试）：
;;   bb --init tools/debug_client.clj -r
;;   => (call "runtime.plugins")        ; socket 从 local.edn 自动推导
;;   => (call "unlock_fps.set" {:unlock_fps true})
;;   => (call "runtime.gates")
;;
;; Socket 解析顺序：显式传参 > SCSP_DEBUG_SOCKET 环境变量 > local.edn 派生
;; （tools/local_config.clj：游戏 .app → Info.plist bundle id → 容器
;; Documents/shiny-song-tools/d.sock）。
;;
;; 响应始终是完整 JSON-RPC map（:id / :result / :error），`:error` 不抛异常，
;; 由调用方判读；传输失败（socket 不存在、连接被拒、连接中断）抛 ex-info。

(ns debug-client
  (:require [cheshire.core :as json]
            [clojure.java.io :as io]
            [local-config :as local-config])
  (:import (java.nio ByteBuffer)
           (java.nio.channels SocketChannel)
           (java.net StandardProtocolFamily UnixDomainSocketAddress)))

(defonce ^:private default-socket
  (atom (some-> (System/getenv "SCSP_DEBUG_SOCKET"))))

(defn set-default-socket!
  "覆盖默认 socket 路径（传 nil 恢复为 local.edn 推导）。"
  [path]
  (reset! default-socket path)
  path)

(defn resolve-socket
  "默认 socket 路径：环境变量 SCSP_DEBUG_SOCKET > local.edn 推导。"
  []
  (or @default-socket
      (-> (local-config/load!) :game :debug-socket)))

(defn- read-exact
  "从 channel 读满 n 字节；对端关闭时抛 ex-info。"
  ^ByteBuffer [^SocketChannel channel n]
  (let [buf (ByteBuffer/allocate n)]
    (loop []
      (when (pos? (.remaining buf))
        (let [read (.read channel buf)]
          (when (neg? read)
            (throw (ex-info "debug socket closed by peer"
                            {:reason :closed :socket (.toString channel)})))
          (recur))))
    (.flip buf)))

(defn- write-all! [^SocketChannel channel ^ByteBuffer buf]
  (while (.hasRemaining buf)
    (.write channel buf)))

(defn- request-frame ^ByteBuffer [method params id]
  (let [body (json/generate-string
              {:jsonrpc "2.0" :id id :method method :params params})]
    (doto (ByteBuffer/allocate (+ 4 (count body)))
      (.putInt (count body))
      (.put (.getBytes body "UTF-8"))
      (.flip))))

(defn- read-response [^SocketChannel channel]
  (let [length (.getInt (read-exact channel 4))
        body (read-exact channel length)]
    (json/parse-string (String. (.array body) "UTF-8") true)))

(defonce ^:private request-id (atom 0))

(defn call
  "调用一个 debug topic，返回完整 JSON-RPC 响应 map（关键字化）。

     (call method)                    ; socket 自动解析（local.edn）
     (call method params)             ; params 是 Clojure map，转 JSON
     (call socket-path method params) ; 显式 socket

  有 :error 时也原样返回（配合 :error 判读）；传输失败抛 ex-info，
  :reason 为 :no-socket / :socket-missing / :closed。"
  ([method] (call (resolve-socket) method {}))
  ([a b]
   (if (map? b)
     (call (resolve-socket) a b)
     (call a b {})))
  ([socket method params]
   (let [path (or socket (resolve-socket))]
     (when-not path
       (throw (ex-info "未指定 debug socket；填写 local.edn 或传显式路径"
                       {:reason :no-socket})))
     (when-not (.exists (io/file path))
       (throw (ex-info (str "debug socket 不存在: " path
                            "（游戏未运行，或 scsp.toml 未开启 debug.enabled）")
                       {:reason :socket-missing :path path})))
     (let [id (swap! request-id inc)]
       (with-open [channel (doto (SocketChannel/open (StandardProtocolFamily/UNIX))
                             (.connect (UnixDomainSocketAddress/of path)))]
         (write-all! channel (request-frame method params id))
         (read-response channel))))))
