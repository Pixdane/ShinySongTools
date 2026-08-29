;; local.edn 加载与校验（docs/tasks.md「本地游戏配置」）。
;;
;; local.edn 在仓库根目录、被 .gitignore 排除，声明本机游戏位置：
;;   {:game {:app "/absolute/path/to/Game.app"}}
;;
;; `load!` 是唯一公共入口：
;; * local.edn 不存在 → 创建空白模板并抛 :local-config/template-created；
;; * 内容不合法 → 抛 :local-config/invalid（:issues 列出具体字段错误）；
;; * 合法 → 返回已校验并派生的游戏上下文：
;;     {:game {:app           "/canonical/path/to/Game.app"
;;             :bundle-id     "<从 Info.plist 推导>"
;;             :documents     ".../Containers/<bundle-id>/Data/Documents"
;;             :debug-socket  ".../Documents/shiny-song-tools/debug.sock"
;;             :bundle        ".../PlugIns/AKInterface.bundle"}}
;;   派生值只存在于返回结果中，不写回 local.edn。
;;
;; Bundle 布局：PlayCover 装的是 iOS 扁平 .app（Info.plist 在 bundle 根），
;; 兼容 macOS 布局（Contents/Info.plist）；PlugIns 路径按检测到的布局派生。

(ns local-config
  (:require [clojure.edn :as edn]
            [clojure.java.io :as io]
            [clojure.java.shell :as shell]
            [clojure.string :as str]))

(def ^:private config-path "local.edn")

(def ^:private template "{:game\n {:app \"\"}}\n")

(defn- absolute-path? [s]
  (and (string? s) (.isAbsolute (io/file s))))

(defn- existing-dir? [s]
  (let [f (io/file s)]
    (and (.isAbsolute f) (.exists f) (.isDirectory f))))

(defn- plist-path
  "Info.plist：iOS 扁平布局在 bundle 根，macOS 布局在 Contents/ 下。"
  [app]
  (let [root (io/file app "Info.plist")
        contents (io/file app "Contents" "Info.plist")]
    (cond (.isFile root) (.getPath root)
          (.isFile contents) (.getPath contents)
          :else nil)))

(defn- plist-value
  "从 Info.plist 读取一个顶层键（plutil 是 macOS 系统自带工具）。"
  [app key]
  (let [plist (or (plist-path app)
                  (throw (ex-info "Info.plist 不存在（根与 Contents/ 均未找到）"
                                  {:app app})))]
    (let [{:keys [exit out err]}
          (shell/sh "plutil" "-extract" key "raw" plist)]
      (when-not (zero? exit)
        (throw (ex-info (str "无法读取 " key ": " err)
                        {:app app :plist plist})))
      (str/trim out))))

(defn- derive [app]
  (let [id            (plist-value app "CFBundleIdentifier")
        exec-name     (plist-value app "CFBundleExecutable")
        documents     (str (System/getProperty "user.home")
                           "/Library/Containers/" id "/Data/Documents")
        plug-ins      (if (.isDirectory (io/file app "Contents"))
                        (str app "/Contents/PlugIns")
                        (str app "/PlugIns"))]
    {:app           app
     :bundle-id     id
     :executable-name exec-name
     :documents    documents
     :debug-socket (str documents "/shiny-song-tools/debug.sock")
     :bundle       (str plug-ins "/AKInterface.bundle")}))

(defn- validate [config]
  (let [issues (volatile! [])]
    (letfn [(issue! [msg] (vswap! issues conj msg))]
      (if-not (map? config)
        (issue! "顶层必须是 EDN map")
        (do
          (doseq [k (keys config)]
            (when-not (contains? #{:game} k)
              (issue! (str "顶层只允许声明 :game，发现: " k))))
          (if-not (map? (:game config))
            (issue! ":game 必须是 map")
            (do
              (doseq [k (keys (:game config))]
                (when-not (contains? #{:app} k)
                  (issue! (str ":game 只允许声明 :app，发现: " k))))
              (let [app (:app (:game config))]
                (if (or (not (string? app)) (str/blank? app))
                  (issue! ":game/:app 必须是非空字符串")
                  (do
                    (when-not (absolute-path? app)
                      (issue! ":game/:app 必须是绝对路径"))
                    (when (and (absolute-path? app) (not (existing-dir? app)))
                      (issue! (str "路径不存在或不是目录: " app)))
                    (when (and (existing-dir? app)
                               (not (str/ends-with? app ".app")))
                      (issue! "路径必须指向 .app bundle"))))))))))
    (vec @issues)))

(defn load!
  "加载并校验 local.edn，返回派生的游戏上下文。失败抛 ex-info：
   {:type :local-config/template-created | :local-config/invalid, ...}"
  []
  (let [file (io/file config-path)]
    (when-not (.exists file)
      (spit config-path template)
      (throw (ex-info "local.edn 不存在；已创建空白模板，请填写 :game/:app 后重试"
                      {:type :local-config/template-created
                       :path config-path})))
    (let [config (try
                   (edn/read-string (slurp config-path))
                   (catch Exception e
                     (throw (ex-info (str "local.edn 无法解析: " (.getMessage e))
                                     {:type :local-config/invalid
                                      :path config-path
                                      :issues [(.getMessage e)]}))))
          issues (validate config)]
      (when (seq issues)
        (throw (ex-info "local.edn 校验失败"
                        {:type :local-config/invalid
                         :path config-path
                         :issues issues})))
      (let [app (:app (:game config))
            canonical (.getCanonicalPath (io/file app))]
        {:game (assoc (derive canonical) :app canonical)}))))
