;; Bundle 安装事务（docs/tasks.md「bundle/status/patch/restore」）。
;;
;; 三个身份（docs「Bundle 身份」）：
;;   baseline  首次受控 patch 前备份的游戏原始 bundle（artifacts/bundle/baseline/）
;;   installed 当前位于游戏 PlugIns 的 bundle
;;   candidate build/AKInterface.bundle + 相邻 sidecar manifest
;;
;; fingerprint：BundleFingerprintV1——有序 [相对 POSIX 路径, SHA-256] 条目向量，
;; 含 _CodeSignature；拒绝 symlink 与非常规条目；结构化相等比较。
;;
;; 事务：同卷原子重命名（.stage-* → 正式路径，installed → .old-*）；
;; current.edn 记录未完成事务；patch/restore 在 drifted/interrupted 状态拒绝；
;; 换入后最终验证失败必须把 old 路径回滚回来；staged bundle 永不重签名。
;;
;; `selftest!` 在 build/ 下构造沙箱 .app + 候选，全生命周期演练
;; （patch → patched → 幂等 → drift 拒绝 → restore → clean → interrupted 拒绝），
;; 不接触真实游戏。

(ns bundle-ops
  (:require [babashka.fs :as fs]
            [cheshire.core :as json]
            [clojure.edn :as edn]
            [clojure.java.io :as io]
            [clojure.java.shell :as shell]
            [clojure.string :as str]
            [local-config :as local-config])
  (:import (java.security MessageDigest)))

;; ---------------------------------------------------------------------------
;; primitives
;; ---------------------------------------------------------------------------

(defn- hex [^bytes bytes]
  (str/join (map #(format "%02x" %) bytes)))

(defn- sha256-file [p]
  (let [digest (MessageDigest/getInstance "SHA-256")]
    (with-open [in (io/input-stream (io/file p))]
      (let [buf (byte-array 65536)]
        (loop []
          (let [n (.read in buf)]
            (when (pos? n)
              (.update digest buf 0 n)
              (recur))))))
    (hex (.digest digest))))

(defn- utf8-compare
  "UTF-8 字节序比较（无符号字节逐位比较）。"
  [a b]
  (let [ba (.getBytes ^String a "UTF-8")
        bb (.getBytes ^String b "UTF-8")]
    (loop [i 0]
      (cond
        (and (< i (alength ba)) (< i (alength bb)))
        (let [x (bit-and (aget ba i) 0xFF)
              y (bit-and (aget bb i) 0xFF)]
          (if (= x y) (recur (inc i)) (compare x y)))
        :else (compare (alength ba) (alength bb))))))

(defn- walk-files
  "目录下全部常规文件的 java.io.File（拒绝 symlink 与非常规条目）。
   file-seq 自身已递归整棵树，这里只做分类与校验，不得再手动递归。"
  [dir]
  (let [root (io/file dir)
        result (volatile! [])]
    (doseq [entry (file-seq root)]
      (when-not (= entry root)
        (when (fs/sym-link? entry)
          (throw (ex-info "bundle 内不允许 symlink" {:path (str entry)})))
        (cond
          (.isFile entry) (vswap! result conj entry)
          (.isDirectory entry) nil
          :else (throw (ex-info "bundle 内存在非常规条目" {:path (str entry)})))))
    @result))

(defn- rel-posix
  "path 相对 root 的 POSIX 路径（root/path 均为本函数调用方产出的路径字符串；
  macOS 分隔符即 /，仓库目标平台固定 darwin）。"
  [root path]
  (let [root (str root)
        path (str path)
        prefix (if (str/ends-with? root "/") root (str root "/"))]
    (when-not (str/starts-with? path prefix)
      (throw (ex-info "path 不在 root 之下" {:root root :path path})))
    (let [rel (subs path (count prefix))]
      (when (or (str/blank? rel) (str/starts-with? rel "/") (str/includes? rel ".."))
        (throw (ex-info "非法相对路径" {:root root :path rel})))
      rel)))

;; ---------------------------------------------------------------------------
;; fingerprint
;; ---------------------------------------------------------------------------

(defn compute-fingerprint
  "目录的 BundleFingerprintV1：{:entries [[path sha]...] :executables {文件名 路径}}。
  entries 有序（UTF-8 字节序），结构化相等即身份相等。"
  [dir]
  (let [root (str dir)
        entries (vec (sort-by first utf8-compare
                              (map (fn [f]
                                     [(rel-posix root (str f)) (sha256-file (str f))])
                                   (walk-files dir))))
        execs (into {} (map (fn [[p _]]
                              [(last (str/split p #"/")) (str (io/file dir p))])
                            entries))]
    {:entries entries :executables execs}))

(defn fingerprint-digest
  "条目向量的摘要（仅用于展示；身份比较用 entries 结构化相等）。"
  [entries]
  (hex (.digest (MessageDigest/getInstance "SHA-256")
                (.getBytes ^String
                           (str/join "\n" (map (fn [[p s]] (str p " " s)) entries))
                           "UTF-8"))))

(defn- entries-from-manifest [manifest]
  (mapv (fn [e] [(get e "path") (get e "sha256")])
        (get-in manifest ["fingerprint" "entries"])))

(defn verify-candidate
  "candidate = {bundle manifest}：重算 fingerprint 与 sidecar 结构化比较，
  并重算 executable SHA-256 与 manifest 一致。
  返回 {:ok true :executable-sha :entries :manifest} 或 {:ok false :issues [...]。"
  [{:keys [candidate-bundle candidate-manifest]}]
  (let [issues (volatile! [])]
    (when-not (and (.exists (io/file candidate-bundle))
                   (.exists (io/file candidate-manifest)))
      (vswap! issues conj "candidate bundle 或 sidecar manifest 不存在"))
    (if (seq @issues)
      {:ok false :issues @issues}
      (let [manifest (json/parse-string (slurp candidate-manifest))
            expected-exec (get-in manifest ["bundle" "executable"])
            expected-exec-sha (get-in manifest ["bundle" "executable_sha256"])
            fp (compute-fingerprint candidate-bundle)
            actual-exec-sha (some-> (get (:executables fp) expected-exec) sha256-file)]
        (when-not (= (entries-from-manifest manifest) (:entries fp))
          (vswap! issues conj "重算 fingerprint 与 sidecar 条目不一致"))
        (when-not (and expected-exec-sha (= expected-exec-sha actual-exec-sha))
          (vswap! issues conj "executable SHA-256 与 manifest 不一致"))
        (cond-> {:ok (empty? @issues)
                 :issues @issues
                 :executable-sha expected-exec-sha
                 :entries (:entries fp)}
          (empty? @issues) (assoc :manifest manifest))))))

(defn codesign-ok?
  "`codesign --verify --strict`。沙箱 fixture 用 :skip-codesign 跳过。"
  [{:keys [skip-codesign] :as _ctx} path]
  (or skip-codesign
      (zero? (:exit (shell/sh "/usr/bin/codesign" "--verify" "--strict" (str path))))))

;; ---------------------------------------------------------------------------
;; state / residue
;; ---------------------------------------------------------------------------

(defn- state-file [ctx] (io/file (:artifacts ctx) "state.edn"))
(defn- baseline-dir [ctx] (io/file (:artifacts ctx) "baseline" "AKInterface.bundle"))
(defn- current-txn [ctx] (io/file (:artifacts ctx) "transactions" "current.edn"))
(defn- history-dir [ctx] (io/file (:artifacts ctx) "transactions" "history"))

(defn- read-state [ctx]
  (let [f (state-file ctx)]
    (when (.exists f)
      (edn/read-string (slurp f)))))

(defn- write-state! [ctx state]
  (.. (history-dir ctx) mkdirs)
  (spit (state-file ctx) (pr-str state)))

(defn- stage-path [ctx] (io/file (.getParentFile (io/file (:bundle ctx))) ".stage-AKInterface.bundle"))
(defn- old-path [ctx] (io/file (.getParentFile (io/file (:bundle ctx))) ".old-AKInterface.bundle"))

(defn- residue-kinds [ctx]
  (remove nil?
          [(when (.exists (current-txn ctx)) "transaction")
           (when (.exists (stage-path ctx)) "stage")
           (when (.exists (old-path ctx)) "old")]))

(defn- game-running? [ctx]
  (when-let [exec-name (:executable-name ctx)]
    (zero? (:exit (shell/sh "pgrep" "-x" exec-name)))))

(defn- installed-entries [ctx]
  (when (.exists (io/file (:bundle ctx)))
    (:entries (compute-fingerprint (:bundle ctx)))))

(defn- installed-exec-sha [ctx]
  (sha256-file (get (:executables (compute-fingerprint (:bundle ctx))) "AKInterface")))

;; ---------------------------------------------------------------------------
;; 上下文
;; ---------------------------------------------------------------------------

(defn game-ctx!
  "真实游戏上下文：local.edn 推导 + 固定的 candidate/artifacts 路径。"
  []
  (let [game (:game (local-config/load!))]
    {:skip-codesign false
     :app (:app game)
     :bundle (:bundle game)
     :executable-name (:executable-name game)
     :candidate-bundle "build/AKInterface.bundle"
     :candidate-manifest "build/AKInterface.bundle.manifest.json"
     :artifacts "artifacts/bundle"}))

;; ---------------------------------------------------------------------------
;; status
;; ---------------------------------------------------------------------------

(defn status
  "只读状态（docs「状态判定」）。见 bb.edn bundle/status 输出格式。"
  [ctx]
  (let [running (boolean (game-running? ctx))
        residue (vec (residue-kinds ctx))
        book (read-state ctx)
        installed-entries (installed-entries ctx)
        installed-exec (when installed-entries
                         (try (installed-exec-sha ctx) (catch Exception _ nil)))
        baseline-entries (some-> book :baseline :entries)
        baseline-exec (some-> book :baseline :executable-sha)
        candidate (when (.exists (io/file (:candidate-manifest ctx)))
                    (try (verify-candidate ctx)
                         (catch Exception e {:ok false :issues [(.getMessage e)]})))
        state-name (cond
                     (seq residue) :interrupted
                     (nil? baseline-entries) :unmanaged
                     (= baseline-entries installed-entries) :clean
                     (and (some-> book :last-install :entries)
                          (= (get-in book [:last-install :entries]) installed-entries)) :patched
                     :else :drifted)
        candidate-status (cond
                           (nil? candidate) :not-built
                           (:ok candidate) (if (= (:entries candidate) installed-entries)
                                             :installed
                                             :update-available)
                           :else :invalid)
        signature (cond
                    (:skip-codesign ctx) :skipped
                    (nil? installed-entries) :invalid
                    (codesign-ok? ctx (:bundle ctx)) :valid
                    :else :invalid)]
    {:game-running running
     :state state-name
     :residue residue
     :installed-exec installed-exec
     :baseline-exec baseline-exec
     :candidate-exec (some-> candidate :executable-sha)
     :candidate-status candidate-status
     :signature signature}))

(defn status-lines
  "status 的人类可读输出（docs 示例格式）。"
  [st]
  (let [line (fn [label value] (format "%-17s %s" label value))]
    (into ["Bundle status" ""]
          [(line "Game" (if (:game-running st) "running" "stopped"))
           (line "State" (name (:state st)))
           (line "Installed exec" (or (:installed-exec st) "-"))
           (line "Baseline exec" (or (:baseline-exec st) "-"))
           (line "Candidate exec" (or (:candidate-exec st) "-"))
           (line "Candidate status" (name (:candidate-status st)))
           (line "Signature" (name (:signature st)))
           (line "Residue" (if (seq (:residue st)) (str/join "," (:residue st)) "none"))])))

;; ---------------------------------------------------------------------------
;; copy / move / txn
;; ---------------------------------------------------------------------------

(defn- copy-tree! [src dst]
  (fs/create-dirs dst)
  (fs/copy-tree (str src) (str dst) {:replace-existing true}))

(defn- atomic-move! [src dst]
  (fs/move (str src) (str dst) {:replace-existing true :atomic-move true}))

(defn- delete-tree! [path]
  (when (.exists (io/file path))
    (fs/delete-tree path)))

(defn- begin-txn! [ctx op extra]
  (.. (history-dir ctx) mkdirs)
  (spit (current-txn ctx)
        (pr-str (merge {:op op
                        :started-at (str (java.time.Instant/now))
                        :bundle (str (:bundle ctx))
                        :stage (str (stage-path ctx))
                        :old (str (old-path ctx))}
                       extra))))

(defn- finish-txn! [ctx record]
  (.. (history-dir ctx) mkdirs)
  (spit (io/file (history-dir ctx)
                 (str (System/currentTimeMillis) "-" (name (:op record)) ".edn"))
        (pr-str record))
  (.delete (current-txn ctx)))

(defn- rollback-to-old!
  "old 路径存在时：丢弃换入内容并把 old 原样移回正式路径。"
  [ctx]
  (when (.exists (old-path ctx))
    (delete-tree! (:bundle ctx))
    (atomic-move! (old-path ctx) (io/file (:bundle ctx)))))

(defn- cleanup-residue! [ctx]
  (delete-tree! (stage-path ctx))
  (delete-tree! (old-path ctx)))

(defn- refuse! [msg data]
  (throw (ex-info (str "拒绝执行: " msg) (assoc data :type :bundle/refused))))

(defn- swap-and-verify!
  "同卷事务换入：installed → .old，stage → installed，最终验证（fingerprint +
   签名）；失败时回滚 old。"
  [ctx expected-entries]
  (when (.exists (io/file (:bundle ctx)))
    (atomic-move! (:bundle ctx) (old-path ctx)))
  (try
    (atomic-move! (stage-path ctx) (io/file (:bundle ctx)))
    (when-not (= expected-entries (:entries (compute-fingerprint (:bundle ctx))))
      (throw (ex-info "换入后 fingerprint 验证失败" {:type :bundle/swap-failed})))
    (when-not (codesign-ok? ctx (:bundle ctx))
      (throw (ex-info "换入后签名验证失败" {:type :bundle/swap-failed})))
    (catch Exception e
      (rollback-to-old! ctx)
      (delete-tree! (stage-path ctx))
      (throw e))))

;; ---------------------------------------------------------------------------
;; patch / restore
;; ---------------------------------------------------------------------------

(defn patch!
  "安装 candidate 到游戏 PlugIns。全部前置条件（docs「patch」）通过后才动
   游戏目录；换入后验证失败必须回滚 old。"
  [ctx {:keys [expected-executable-sha expected-installed-executable-sha]}]
  (when (game-running? ctx)
    (refuse! "游戏正在运行" {:state :running}))
  (when-not (.exists (io/file (:bundle ctx)))
    (refuse! "游戏 PlugIns 中未找到 AKInterface.bundle" {}))
  (let [cand (verify-candidate ctx)]
    (when-not (:ok cand)
      (refuse! "candidate 校验失败" {:issues (:issues cand)}))
    (when-not (codesign-ok? ctx (:candidate-bundle ctx))
      (refuse! "candidate 签名验证失败" {}))
    (when-not (and expected-executable-sha
                   (= expected-executable-sha (:executable-sha cand)))
      (refuse! "--expected-executable-sha 与 candidate 不一致"
               {:expected expected-executable-sha
                :actual (:executable-sha cand)}))
    (let [book (read-state ctx)
          state (status ctx)
          installed-entries (installed-entries ctx)]
      (when (or (= :interrupted (:state state)) (= :drifted (:state state)))
        (refuse! (str "当前状态 " (name (:state state)) "，先解决事务残留/漂移")
                 {:state (:state state) :residue (:residue state)}))
      ;; ---- 前置检查通过，进入事务 ----
      (begin-txn! ctx :patch {:expected-executable-sha expected-executable-sha})
      (try
        (when (and (some-> book :last-install :entries)
                   (= (get-in book [:last-install :entries]) installed-entries)
                   (= (:entries cand) installed-entries))
          ;; docs：installed 同时匹配该 candidate 和最后一次成功安装记录 → 幂等成功
          (throw (ex-info "idempotent" {:type ::patch-idempotent})))
        (when-not (:baseline book)
          ;; 首次 patch：建立 baseline（复制后再次验证，docs 要求）
          (when-not expected-installed-executable-sha
            (refuse! "首次 patch 需要提供 --expected-installed-executable-sha 以建立 baseline" {}))
          (let [installed-sha (installed-exec-sha ctx)]
            (when-not (= expected-installed-executable-sha installed-sha)
              (refuse! "--expected-installed-executable-sha 与当前 installed 不一致"
                       {:expected expected-installed-executable-sha
                        :actual installed-sha}))
            (.. (baseline-dir ctx) getParentFile mkdirs)
            (delete-tree! (baseline-dir ctx))
            (copy-tree! (:bundle ctx) (baseline-dir ctx))
            (let [baseline-fp (:entries (compute-fingerprint (baseline-dir ctx)))]
              (when-not (= baseline-fp installed-entries)
                (refuse! "baseline 复制后 fingerprint 不一致" {}))
              (write-state! ctx {:baseline {:entries baseline-fp
                                            :executable-sha installed-sha}}))))
        ;; stage：复制 candidate → 同卷临时目录，复制后验证（不重签名）
        (delete-tree! (stage-path ctx))
        (copy-tree! (:candidate-bundle ctx) (stage-path ctx))
        (let [stage-fp (:entries (compute-fingerprint (stage-path ctx)))]
          (when-not (= stage-fp (:entries cand))
            (refuse! "staged bundle fingerprint 与 candidate 不一致" {}))
          (when-not (codesign-ok? ctx (str (stage-path ctx)))
            (refuse! "staged bundle 签名验证失败" {})))
        (swap-and-verify! ctx (:entries cand))
        ;; 成功：清理 + 记账（baseline 保留，last-install 更新）
        (delete-tree! (old-path ctx))
        (write-state! ctx (assoc (select-keys (read-state ctx) [:baseline])
                                 :last-install {:entries (:entries cand)
                                                :executable-sha (:executable-sha cand)}))
        (finish-txn! ctx {:op :patch :outcome :success
                          :executable-sha (:executable-sha cand)})
        (cleanup-residue! ctx)
        {:ok true :executable-sha (:executable-sha cand)
         :first-patch (nil? (:baseline book))}
        (catch Exception e
          (if (= ::patch-idempotent (:type (ex-data e)))
            (do (finish-txn! ctx {:op :patch :outcome :idempotent})
                {:ok true :idempotent true :executable-sha (:executable-sha cand)})
            (do (rollback-to-old! ctx)
                (finish-txn! ctx {:op :patch :outcome :failed :error (.getMessage e)})
                (throw e))))))))

(defn restore!
  "把 baseline 事务性恢复到游戏 PlugIns（docs「restore」）。"
  [ctx]
  (when (game-running? ctx)
    (refuse! "游戏正在运行" {:state :running}))
  (let [book (read-state ctx)
        baseline-entries (:baseline book)]
    (when-not baseline-entries
      (refuse! "尚未建立受信任 baseline" {}))
    (let [state (status ctx)]
      (when (or (= :interrupted (:state state)) (= :drifted (:state state)))
        (refuse! (str "当前状态 " (name (:state state)))
                 {:state (:state state)}))
      (if (= :clean (:state state))
        ;; 已是 baseline：幂等成功，只报告状态
        {:ok true :idempotent true}
        (do
          (let [baseline (baseline-dir ctx)]
            (when-not (.exists (io/file baseline))
              (refuse! "baseline 目录缺失" {}))
            (let [fp (compute-fingerprint baseline)]
              (when-not (= (:entries fp) (:entries baseline-entries))
                (refuse! "baseline fingerprint 校验失败" {})))
            (when-not (and (some-> book :last-install :entries)
                           (= (get-in book [:last-install :entries])
                              (installed-entries ctx)))
              (refuse! "installed 与最后一次成功安装记录不一致" {})))
          (begin-txn! ctx :restore {})
          (try
            (delete-tree! (stage-path ctx))
            (copy-tree! (baseline-dir ctx) (stage-path ctx))
            (let [stage-fp (:entries (compute-fingerprint (stage-path ctx)))]
              (when-not (= stage-fp (:entries baseline-entries))
                (refuse! "staged baseline fingerprint 不一致" {})))
            (swap-and-verify! ctx (:entries baseline-entries))
            (delete-tree! (old-path ctx))
            (write-state! ctx (select-keys book [:baseline]))
            (finish-txn! ctx {:op :restore :outcome :success})
            (cleanup-residue! ctx)
            {:ok true}
            (catch Exception e
              (rollback-to-old! ctx)
              (finish-txn! ctx {:op :restore :outcome :failed :error (.getMessage e)})
              (throw e))))))))

;; ---------------------------------------------------------------------------
;; selftest（沙箱全生命周期，不接触真实游戏；产物在 build/tmp/ 下）
;; ---------------------------------------------------------------------------

(defn- assert= [expected actual label]
  (when-not (= expected actual)
    (throw (ex-info (str "selftest 失败: " label)
                    {:expected expected :actual actual}))))

(defn- assert-refused [f label]
  (let [e (try (f) nil (catch Exception e e))]
    (when-not (= :bundle/refused (:type (ex-data e)))
      (throw (ex-info (str "selftest 失败（应当拒绝）: " label)
                      {:actual (some-> e ex-data)})))))

(defn- mk-bundle! [dir exec-content]
  (.. (io/file dir "Contents" "MacOS") mkdirs)
  (.. (io/file dir "_CodeSignature") mkdirs)
  (spit (io/file dir "Contents" "MacOS" "AKInterface") exec-content)
  (spit (io/file dir "Contents" "Info.plist") "<?xml version=\"1.0\"?><plist/>")
  (spit (io/file dir "_CodeSignature" "CodeResources") "resource-map")
  (str dir))

(defn- mk-candidate-manifest! [cand-dir manifest-path]
  (let [fp (compute-fingerprint cand-dir)
        exec-sha (sha256-file (get (:executables fp) "AKInterface"))
        manifest {"bundle" {"executable" "AKInterface"
                            "executable_sha256" exec-sha}
                  "fingerprint" {"version" "BundleFingerprintV1"
                                 "entries" (mapv (fn [[p s]] {"path" p "sha256" s})
                                                 (:entries fp))}}]
    (spit manifest-path (json/generate-string manifest))
    exec-sha))

(defn selftest!
  "沙箱演练：unmanaged → patch(拒) → patch → patched → 幂等 patch → drift 拒 →
   修复 → restore → clean → 幂等 restore → interrupted 拒。返回 :ok。"
  []
  (let [root (io/file "build" "tmp"
                      (str "bundle-selftest-" (System/currentTimeMillis)))
        app (io/file root "Game.app")
        plugins (io/file app "PlugIns")
        bundle (io/file plugins "AKInterface.bundle")
        cand (io/file root "candidate" "AKInterface.bundle")
        ctx {:skip-codesign true
             :app (str app)
             :bundle (str bundle)
             :executable-name "Game"
             :candidate-bundle (str cand)
             :candidate-manifest (str cand ".manifest.json")
             :artifacts (str (io/file root "artifacts" "bundle"))}]
    ;; 沙箱：假 .app（iOS 扁平布局）+ 已安装 v1 + 候选 v2
    (.. (io/file app "PlugIns") mkdirs)
    (mk-bundle! bundle "installed-v1")
    (mk-bundle! cand "candidate-v2")
    (let [cand-v2-sha (mk-candidate-manifest! cand (str cand ".manifest.json"))
          v1-sha (sha256-file (io/file bundle "Contents" "MacOS" "AKInterface"))]

      ;; 1. unmanaged + candidate 可用
      (assert= :unmanaged (:state (status ctx)) "初始状态 unmanaged")
      (assert= :update-available (:candidate-status (status ctx))
               "unmanaged 时 candidate 应显示 update-available")

      ;; 2. patch：缺 --expected-executable-sha → 拒绝
      (assert-refused #(patch! ctx {:expected-installed-executable-sha v1-sha})
                      "patch 缺 executable sha")
      ;; 3. patch：executable sha 不匹配 → 拒绝
      (assert-refused #(patch! ctx {:expected-executable-sha "deadbeef"
                                    :expected-installed-executable-sha v1-sha})
                      "patch sha 不匹配")
      ;; 4. patch：缺 --expected-installed-executable-sha（首次）→ 拒绝
      (assert-refused #(patch! ctx {:expected-executable-sha cand-v2-sha})
                      "首次 patch 缺 installed sha")

      ;; 5. patch 成功（建立 baseline + 安装 v2）
      (let [result (patch! ctx {:expected-executable-sha cand-v2-sha
                                :expected-installed-executable-sha v1-sha})]
        (assert= true (:ok result) "patch 成功")
        (assert= true (:first-patch result) "首次 patch 标记"))
      (let [st (status ctx)]
        (assert= :patched (:state st) "patch 后状态 patched")
        (assert= :installed (:candidate-status st) "candidate 已安装")
        (assert= v1-sha (:baseline-exec st) "baseline 记录 v1"))

      ;; 6. 幂等 patch（同 candidate 重复安装）
      (let [result (patch! ctx {:expected-executable-sha cand-v2-sha
                                :expected-installed-executable-sha v1-sha})]
        (assert= true (:idempotent result) "重复 patch 幂等成功"))

      ;; 7. drift：外部篡改 installed → patch/restore 都拒绝
      (spit (io/file bundle "Contents" "MacOS" "AKInterface") "tampered")
      (assert= :drifted (:state (status ctx)) "篡改后 drifted")
      (assert-refused #(patch! ctx {:expected-executable-sha cand-v2-sha})
                      "drifted 状态 patch")
      (assert-refused #(restore! ctx) "drifted 状态 restore")

      ;; 8. 修复（模拟人工恢复 candidate 内容）→ patched → restore → clean
      (spit (io/file bundle "Contents" "MacOS" "AKInterface") "candidate-v2")
      (assert= :patched (:state (status ctx)) "恢复后 patched")
      (let [result (restore! ctx)]
        (assert= true (:ok result) "restore 成功"))
      (let [st (status ctx)]
        (assert= :clean (:state st) "restore 后 clean")
        (assert= v1-sha (:installed-exec st) "installed 回到 baseline v1"))

      ;; 9. 幂等 restore
      (let [result (restore! ctx)]
        (assert= true (:idempotent result) "重复 restore 幂等成功"))

      ;; 10. interrupted：残留 stage 目录 → patch/restore 拒绝
      (.. (stage-path ctx) mkdirs)
      (assert= :interrupted (:state (status ctx)) "残留后 interrupted")
      (assert-refused #(patch! ctx {:expected-executable-sha cand-v2-sha})
                      "interrupted 状态 patch")
      (assert-refused #(restore! ctx) "interrupted 状态 restore")

      (delete-tree! root)
      :ok)))
