;; Lossless translation-template generation for LocalizationManager dumps.
;;
;; The generated catalog is the only file translators edit.  bindings.json is
;; a machine-owned reverse index from every (category,id) slot to a catalog
;; unit plus the concrete wildcard arguments needed to reconstruct that slot.
;; `apply!` refuses source drift and placeholder drift before materializing a
;; complete localify-compatible dictionary.

(ns translation-templates
  (:require [babashka.fs :as fs]
            [cheshire.core :as json]
            [clojure.java.io :as io]
            [clojure.string :as str])
  (:import (java.nio.file Files StandardCopyOption)
           (java.nio.file.attribute PosixFilePermissions)
           (java.security MessageDigest)
           (java.util.regex Pattern)))

(def schema-version 1)
(def default-dump "resources/localization_manager_dic.json")
(def default-output-dir "resources/translation_templates")
(def default-applied "build/translations/localify.json")

(def ^:private original-brace-pattern
  (Pattern/compile "\\{[0-9]+(?::[^}]*)?\\}"))

;; Deliberately accepts actual printf conversions, not strings such as 100%UP.
(def ^:private original-printf-pattern
  (Pattern/compile
   "%(?:[0-9]+\\$)?[-+#0 ']*[0-9]*(?:\\.(?:[0-9]+|\\*))?(?:hh|h|ll|l|j|z|t|L)?[diuoxXfFeEgGaAcspn%]"))

(def ^:private custom-pattern
  (Pattern/compile "\\{\\{sst:(?:number|attribute|rarity|character|event|card):[0-9]+\\}\\}"))

(def ^:private protected-pattern
  (Pattern/compile
   (str "(?:\\{[0-9]+(?::[^}]*)?\\})"
        "|(?:%(?:[0-9]+\\$)?[-+#0 ']*[0-9]*(?:\\.(?:[0-9]+|\\*))?(?:hh|h|ll|l|j|z|t|L)?[diuoxXfFeEgGaAcspn%])"
        "|(?:<[^>]*>)"
        "|(?:\\{\\{sst:[^}]+\\}\\})")))

(def ^:private scalar-pattern
  (Pattern/compile
   "(?<![A-Za-z])(?:SSR|SR|R)(?![A-Za-z])|(?<![A-Za-z])(?:Vo|Da|Vi)(?![A-Za-z])|[0-9０-９]+(?:[.,．，:/：／-][0-9０-９]+)*"))

(def ^:private card-pattern (Pattern/compile "【([^】\\r\\n]+)】"))
(def ^:private quote-patterns
  [(Pattern/compile "「([^」\\r\\n]+)」")
   (Pattern/compile "『([^』\\r\\n]+)』")])
(def ^:private event-context-pattern
  (Pattern/compile
   "イベント|ガシャ|ミッション|キャンペーン|ランキング|Anniversary|アニバーサリー|記念|フェス"))

(defn- hex [^bytes bytes]
  (str/join (map #(format "%02x" (bit-and % 0xff)) bytes)))

(defn- sha256-string [s]
  (let [digest (MessageDigest/getInstance "SHA-256")]
    (.update digest (.getBytes ^String s "UTF-8"))
    (hex (.digest digest))))

(defn- stable-id [prefix & parts]
  (str prefix "_" (subs (sha256-string (str/join "\u0000" parts)) 0 16)))

(defn- atomic-write-json! [path value pretty?]
  (let [dest (.toPath (io/file path))
        parent (.getParent dest)]
    (Files/createDirectories parent (make-array java.nio.file.attribute.FileAttribute 0))
    (let [tmp (Files/createTempFile parent ".translation-templates-" ".tmp"
                                    (make-array java.nio.file.attribute.FileAttribute 0))]
      (try
        (spit (.toFile tmp) (str (json/generate-string value (cond-> {} pretty? (assoc :pretty true))) "\n"))
        (Files/move tmp dest
                    (into-array StandardCopyOption
                                [StandardCopyOption/ATOMIC_MOVE
                                 StandardCopyOption/REPLACE_EXISTING]))
        (try
          (Files/setPosixFilePermissions dest (PosixFilePermissions/fromString "rw-r--r--"))
          (catch UnsupportedOperationException _ nil))
        (finally
          (Files/deleteIfExists tmp))))))

(defn- atomic-write-jsonl! [path values]
  (let [dest (.toPath (io/file path))
        parent (.getParent dest)]
    (Files/createDirectories parent (make-array java.nio.file.attribute.FileAttribute 0))
    (let [tmp (Files/createTempFile parent ".translation-templates-" ".tmp"
                                    (make-array java.nio.file.attribute.FileAttribute 0))]
      (try
        (with-open [writer (io/writer (.toFile tmp) :encoding "UTF-8")]
          (doseq [value values]
            (.write writer (json/generate-string value))
            (.write writer "\n")))
        (Files/move tmp dest
                    (into-array StandardCopyOption
                                [StandardCopyOption/ATOMIC_MOVE
                                 StandardCopyOption/REPLACE_EXISTING]))
        (try
          (Files/setPosixFilePermissions dest (PosixFilePermissions/fromString "rw-r--r--"))
          (catch UnsupportedOperationException _ nil))
        (finally
          (Files/deleteIfExists tmp))))))

(defn- read-json! [path]
  (try
    (json/parse-string (slurp path))
    (catch Exception e
      (throw (ex-info "JSON 无法读取" {:path (str path)} e)))))

(defn- read-dump! [path]
  (let [value (read-json! path)]
    (when-not (map? value)
      (throw (ex-info "dump 顶层必须是 object" {:path (str path)})))
    (doseq [[category entries] value]
      (when-not (and (string? category) (map? entries))
        (throw (ex-info "dump category 必须映射到 object" {:category category})))
      (doseq [[id text] entries]
        (when-not (and (string? id) (string? text))
          (throw (ex-info "dump 槽位必须是 string -> string"
                          {:category category :id id :value text})))))
    value))

(defn- ordered-map [entries]
  (into (sorted-map) entries))

(defn- canonical-dump [dump]
  (ordered-map
   (for [[category entries] dump]
     [category (ordered-map entries)])))

(defn- dump-digest [dump]
  (sha256-string (json/generate-string (canonical-dump dump))))

(defn- matcher-values [^Pattern pattern text]
  (let [matcher (.matcher pattern ^String text)]
    (loop [values []]
      (if (.find matcher)
        (recur (conj values (.group matcher)))
        values))))

(defn- placeholders [text]
  {:original (vec (concat (matcher-values original-brace-pattern text)
                          (matcher-values original-printf-pattern text)))
   :custom (vec (matcher-values custom-pattern text))})

(defn- ensure-no-reserved-syntax! [records]
  (when-let [record (first (filter #(re-find #"\{\{sst:" (:text %)) records))]
    (throw (ex-info "原文包含保留的 {{sst:...}} 语法"
                    (select-keys record [:category :id :text])))))

(defn- dump-records [dump]
  (vec
   (for [[category entries] dump
         [id text] entries
         :when (not (boolean (re-matches #"(?s)[\s\u200B\uFEFF\u3000]*" text)))]
     {:category category :id id :text text})))

(defn- character-aliases [dump]
  (let [values (fn [category]
                 (->> (get dump category {}) vals (remove str/blank?)))
        full (values "mlCharacterText_Name")
        first-names (values "mlCharacterText_FirstName")
        last-names (values "mlCharacterText_LastName")
        compact-full (map #(str/replace % #"[  ]" "") full)]
    (->> (concat full compact-full first-names last-names)
         (remove #(or (str/blank? %) (< (.length ^String %) 2)))
         distinct
         (sort-by (fn [s] [(- (.length ^String s)) s]))
         vec)))

(defn- overlaps? [span spans]
  (some (fn [other]
          (and (< (:start span) (:end other))
               (< (:start other) (:end span))))
        spans))

(defn- pattern-spans [^Pattern pattern text type]
  (let [matcher (.matcher pattern ^String text)]
    (loop [spans []]
      (if (.find matcher)
        (recur (conj spans {:start (.start matcher 1)
                            :end (.end matcher 1)
                            :type type
                            :source (.group matcher 1)}))
        spans))))

(defn- character-pattern [aliases]
  (Pattern/compile
   (str "(?:" (str/join "|" (map #(Pattern/quote ^String %) aliases)) ")")))

(defn- alias-spans [text ^Pattern pattern occupied]
  (let [matcher (.matcher pattern ^String text)]
    (loop [spans []]
      (if (.find matcher)
        (let [span {:start (.start matcher)
                    :end (.end matcher)
                    :type "character"
                    :source (.group matcher)}]
          (recur (if (overlaps? span occupied) spans (conj spans span))))
        spans))))

(defn- event-span? [text {:keys [start end source]} character-set]
  (and (not (contains? character-set source))
       (let [left (subs text (max 0 (- start 32)) start)
             right (subs text end (min (.length ^String text) (+ end 48)))]
         (boolean (re-find event-context-pattern (str left " " right))))))

(defn- select-non-overlapping [spans occupied]
  (reduce (fn [selected span]
            (if (overlaps? span (concat occupied selected))
              selected
              (conj selected span)))
          []
          (sort-by (fn [{:keys [start end]}] [start (- end start)]) spans)))

(defn- entity-spans [text character-pattern character-set]
  (let [cards (pattern-spans card-pattern text "card")
        chars (alias-spans text character-pattern cards)
        occupied (vec (concat cards chars))
        events (->> quote-patterns
                    (mapcat #(pattern-spans % text "event"))
                    (filter #(event-span? text % character-set))
                    (#(select-non-overlapping % occupied)))]
    (vec (sort-by :start (concat occupied events)))))

(defn- token [kind index]
  (str "{{sst:" kind ":" index "}}"))

(defn- replace-entity-spans [text spans]
  (let [out (StringBuilder.)]
    (loop [cursor 0
           spans spans
           counters {}
           args {}]
      (if-let [{:keys [start end type source]} (first spans)]
        (let [index (get counters type 0)
              placeholder (token type index)]
          (.append out (subs text cursor start))
          (.append out placeholder)
          (recur end (next spans) (update counters type (fnil inc 0))
                 (assoc args placeholder {:kind :entity
                                          :entity-type type
                                          :source source})))
        (do
          (.append out (subs text cursor))
          {:text (str out) :args args :counters counters})))))

(defn- scalar-kind [value]
  (cond
    (re-matches #"[0-9０-９]+(?:[.,．，:/：／-][0-9０-９]+)*" value) "number"
    (#{"Vo" "Da" "Vi"} value) "attribute"
    :else "rarity"))

(defn- normalize-plain [text counters args]
  (let [matcher (.matcher scalar-pattern ^String text)
        out (StringBuilder.)]
    (loop [cursor 0 counters counters args args]
      (if (.find matcher)
        (let [value (.group matcher)
              kind (scalar-kind value)
              index (get counters kind 0)
              placeholder (token kind index)]
          (.append out (subs text cursor (.start matcher)))
          (.append out placeholder)
          (recur (.end matcher)
                 (update counters kind (fnil inc 0))
                 (assoc args placeholder {:kind :literal :value value})))
        (do
          (.append out (subs text cursor))
          {:text (str out) :counters counters :args args})))))

(defn- normalize-scalars [text counters args]
  (let [matcher (.matcher protected-pattern ^String text)
        out (StringBuilder.)]
    (loop [cursor 0 counters counters args args]
      (if (.find matcher)
        (let [plain (normalize-plain (subs text cursor (.start matcher)) counters args)]
          (.append out (:text plain))
          (.append out (.group matcher))
          (recur (.end matcher) (:counters plain) (:args plain)))
        (let [plain (normalize-plain (subs text cursor) counters args)]
          (.append out (:text plain))
          {:template (str out) :args (:args plain)})))))

(defn- candidate [record character-pattern character-set]
  (let [entities (replace-entity-spans (:text record)
                                       (entity-spans (:text record)
                                                     character-pattern
                                                     character-set))
        normalized (normalize-scalars (:text entities) (:counters entities) (:args entities))]
    (assoc record :candidate (:template normalized) :candidate-args (:args normalized))))

(defn- qualified-templates [records]
  (->> records
       (filter #(seq (:candidate-args %)))
       (group-by :candidate)
       (keep (fn [[template group]]
               (when (>= (count (distinct (map :text group))) 2) template)))
       set))

(defn- entity-id [{:keys [entity-type source]}]
  (stable-id "e" entity-type source))

(defn- template-id [source]
  (stable-id "t" source))

(defn- material-record [record qualified]
  (let [base (select-keys record [:category :id :text])]
    (if (contains? qualified (:candidate record))
      (let [args (ordered-map
                  (for [[placeholder arg] (:candidate-args record)]
                    [placeholder
                     (if (= :entity (:kind arg))
                       {"entity" (entity-id arg)}
                       (:value arg))]))]
        (assoc base
               :unit-source (:candidate record)
               :args args
               :used-entity-args (filterv #(= :entity (:kind %))
                                          (vals (:candidate-args record)))))
      (assoc base :unit-source (:text record) :args (sorted-map) :used-entity-args []))))

(defn- material-records [dump]
  (let [records (dump-records dump)
        _ (ensure-no-reserved-syntax! records)
        aliases (character-aliases dump)
        pattern (character-pattern aliases)
        character-set (set aliases)
        candidates (mapv #(candidate % pattern character-set) records)
        qualified (qualified-templates candidates)]
    (mapv #(material-record % qualified) candidates)))

(defn- old-targets [catalog-path]
  (if (.exists (io/file catalog-path))
    ;; Streaming keeps regeneration bounded even after catalog.json grows to
    ;; tens of thousands of entries. Only completed targets need preserving.
    (with-open [reader (io/reader catalog-path :encoding "UTF-8")]
      (reduce (fn [targets line]
                (if (str/blank? line)
                  targets
                  (let [entry (json/parse-string line)
                        target (get entry "target")]
                    (if (or (= "header" (get entry "record_type")) (nil? target))
                      targets
                      (assoc targets (get entry "id")
                             {:source (get entry "source")
                              :kind (get entry "kind")
                              :target target})))))
              {}
              (line-seq reader)))
    {}))

(defn- read-catalog! [catalog-path]
  (try
    (with-open [reader (io/reader catalog-path :encoding "UTF-8")]
      (let [records (->> (line-seq reader)
                         (remove str/blank?)
                         (mapv json/parse-string))
            header (first records)
            entries (subvec records 1)]
        (when-not (= "header" (get header "record_type"))
          (throw (ex-info "catalog.jsonl 第一行必须是 header"
                          {:path (str catalog-path)})))
        (assoc (dissoc header "record_type") "entries" entries)))
    (catch clojure.lang.ExceptionInfo e
      (throw e))
    (catch Exception e
      (throw (ex-info "catalog.jsonl 无法读取" {:path (str catalog-path)} e)))))

(defn- retained-target [old id kind source]
  (let [entry (get old id)]
    (when (and (= kind (:kind entry)) (= source (:source entry)))
      (:target entry))))

(defn- catalog-entry [old source records]
  (let [id (template-id source)
        ph (placeholders source)
        kind (if (or (seq (:custom ph)) (seq (:original ph))) "template" "text")]
    (ordered-map
     [["id" id]
      ["kind" kind]
      ["source" source]
      ["target" (retained-target old id kind source)]
      ["occurrences" (count records)]
      ["variants" (count (distinct (map :text records)))]
      ["original_placeholders" (:original ph)]
      ["custom_placeholders" (:custom ph)]])))

(defn- entity-entries [old records]
  (let [args (mapcat :used-entity-args records)]
    (->> args
         (group-by (juxt :entity-type :source))
         (map (fn [[[entity-type source] uses]]
                (let [id (entity-id (first uses))]
                  (ordered-map
                   [["id" id]
                    ["kind" "entity"]
                    ["entity_type" entity-type]
                    ["source" source]
                    ["target" (retained-target old id "entity" source)]
                    ["occurrences" (count uses)]
                    ["variants" 1]
                    ["original_placeholders" []]
                    ["custom_placeholders" []]]))))
         (sort-by #(get % "source"))
         vec)))

(defn- binding-value [{:keys [unit-source args]}]
  (cond-> (ordered-map [["unit" (template-id unit-source)]])
    (seq args) (assoc "args" args)))

(defn- atomic-write-bindings! [path records digest]
  (let [dest (.toPath (io/file path))
        parent (.getParent dest)]
    (Files/createDirectories parent (make-array java.nio.file.attribute.FileAttribute 0))
    (let [tmp (Files/createTempFile parent ".translation-bindings-" ".tmp"
                                    (make-array java.nio.file.attribute.FileAttribute 0))]
      (try
        (with-open [writer (io/writer (.toFile tmp) :encoding "UTF-8")]
          (.write writer "{\"bindings\":{")
          (doseq [[category-index group] (map-indexed vector (partition-by :category records))]
            (when (pos? category-index) (.write writer ","))
            (.write writer (json/generate-string (:category (first group))))
            (.write writer ":{")
            (doseq [[entry-index record] (map-indexed vector group)]
              (when (pos? entry-index) (.write writer ","))
              (.write writer (json/generate-string (:id record)))
              (.write writer ":")
              (.write writer (json/generate-string (binding-value record))))
            (.write writer "}"))
          (.write writer "},\"schema_version\":")
          (.write writer (str schema-version))
          (.write writer ",\"slots\":")
          (.write writer (str (count records)))
          (.write writer ",\"source_digest\":")
          (.write writer (json/generate-string digest))
          (.write writer "}\n"))
        (Files/move tmp dest
                    (into-array StandardCopyOption
                                [StandardCopyOption/ATOMIC_MOVE
                                 StandardCopyOption/REPLACE_EXISTING]))
        (try
          (Files/setPosixFilePermissions dest (PosixFilePermissions/fromString "rw-r--r--"))
          (catch UnsupportedOperationException _ nil))
        (finally
          (Files/deleteIfExists tmp))))))

(defn generate!
  ([] (generate! default-dump default-output-dir))
  ([dump-path output-dir]
   (let [dump (read-dump! dump-path)
         material (material-records dump)
         catalog-path (str (io/file output-dir "catalog.jsonl"))
         bindings-path (str (io/file output-dir "bindings.json"))
         old (old-targets catalog-path)
         text-entries (->> material
                           (group-by :unit-source)
                           (map (fn [[source group]] (catalog-entry old source group)))
                           (sort-by #(get % "source"))
                           vec)
         entities (entity-entries old material)
         entries (vec (concat entities text-entries))
         digest (dump-digest dump)
         catalog-header (ordered-map
                         [["record_type" "header"]
                          ["schema_version" schema-version]
                          ["source_digest" digest]
                          ["wildcards"
                           [(ordered-map [["syntax" "{0} / {0:D2} / %s / %d"]
                                          ["kind" "original"]
                                          ["rule" "游戏原版占位符；译文必须原样保留"]])
                            (ordered-map [["syntax" "{{sst:number:N}}"] ["kind" "literal"] ["rule" "数字、日期或时间"]])
                            (ordered-map [["syntax" "{{sst:attribute:N}}"] ["kind" "literal"] ["rule" "Vo / Da / Vi"]])
                            (ordered-map [["syntax" "{{sst:rarity:N}}"] ["kind" "literal"] ["rule" "R / SR / SSR"]])
                            (ordered-map [["syntax" "{{sst:character:N}}"] ["kind" "entity"] ["rule" "角色名；译文来自独立 entity 单元"]])
                            (ordered-map [["syntax" "{{sst:event:N}}"] ["kind" "entity"] ["rule" "活动名；译文来自独立 entity 单元"]])
                            (ordered-map [["syntax" "{{sst:card:N}}"] ["kind" "entity"] ["rule" "卡片名；译文来自独立 entity 单元"]])]]])]
     (atomic-write-jsonl! catalog-path (cons catalog-header entries))
     (atomic-write-bindings! bindings-path material digest)
     {:catalog catalog-path
      :bindings bindings-path
      :slots (count material)
      :units (count text-entries)
      :entities (count entities)
      :templates (count (filter #(= "template" (get % "kind")) text-entries))
      :pending (count (filter #(nil? (get % "target")) entries))
      :source-digest digest})))

(defn- catalog-index! [catalog]
  (when-not (= schema-version (get catalog "schema_version"))
    (throw (ex-info "catalog schema_version 不支持"
                    {:expected schema-version :actual (get catalog "schema_version")})))
  (let [entries (get catalog "entries")
        grouped (group-by #(get % "id") entries)]
    (when-let [[id duplicates] (first (filter #(> (count (val %)) 1) grouped))]
      (throw (ex-info "catalog id 重复" {:id id :count (count duplicates)})))
    (into {} (map (fn [entry] [(get entry "id") entry]) entries))))

(defn- replacement-value [index arg translated?]
  (if (string? arg)
    arg
    (let [entity (get index (get arg "entity"))]
      (when-not (= "entity" (get entity "kind"))
        (throw (ex-info "binding 引用了不存在的 entity" {:argument arg})))
      (let [value (if translated? (get entity "target") (get entity "source"))]
        (when (and translated? (nil? value))
          (throw (ex-info "entity 尚未翻译" {:id (get entity "id")
                                             :source (get entity "source")})))
        value))))

(defn- placeholder-frequencies [values]
  (frequencies values))

(defn- validate-target-placeholders! [entry]
  (let [target (get entry "target")]
    (when-not (string? target)
      (throw (ex-info "翻译单元尚未填写 target"
                      {:id (get entry "id") :source (get entry "source")})))
    (let [source-ph (placeholders (get entry "source"))
          target-ph (placeholders target)]
      (when-not (= (placeholder-frequencies (:original source-ph))
                   (placeholder-frequencies (:original target-ph)))
        (throw (ex-info "译文改变了游戏原版占位符"
                        {:id (get entry "id")
                         :source (:original source-ph)
                         :target (:original target-ph)})))
      (when-not (= (placeholder-frequencies (:custom source-ph))
                   (placeholder-frequencies (:custom target-ph)))
        (throw (ex-info "译文改变了自制占位符"
                        {:id (get entry "id")
                         :source (:custom source-ph)
                         :target (:custom target-ph)}))))))

(defn- expand-unit [index entry args translated?]
  (let [text (if translated? (get entry "target") (get entry "source"))]
    (when translated? (validate-target-placeholders! entry))
    (reduce (fn [result placeholder]
              (let [arg (get args placeholder ::missing)]
                (when (= ::missing arg)
                  (throw (ex-info "binding 缺少自制占位符实参"
                                  {:id (get entry "id") :placeholder placeholder})))
                (str/replace result placeholder (replacement-value index arg translated?))))
            text
            (get entry "custom_placeholders"))))

(defn- validate-source! [dump bindings index]
  (when-not (= (get bindings "source_digest") (dump-digest dump))
    (throw (ex-info "dump 与 bindings 的 source_digest 不一致"
                    {:expected (get bindings "source_digest")
                     :actual (dump-digest dump)})))
  (let [slot-bindings (get bindings "bindings")]
    (doseq [[category entries] dump
            [id source] entries
            :when (not (boolean (re-matches #"(?s)[\s\u200B\uFEFF\u3000]*" source)))]
      (let [binding (get-in slot-bindings [category id])
            entry (get index (get binding "unit"))]
        (when-not binding
          (throw (ex-info "非空 dump 槽位没有 binding" {:category category :id id})))
        (when-not entry
          (throw (ex-info "binding 引用了不存在的翻译单元"
                          {:category category :id id :unit (get binding "unit")})))
        (let [rebuilt (expand-unit index entry (get binding "args" {}) false)]
          (when-not (= source rebuilt)
            (throw (ex-info "binding 无法无损重建原文"
                            {:category category :id id :expected source :actual rebuilt})))))))
  true)

(defn check!
  ([] (check! default-dump default-output-dir))
  ([dump-path output-dir]
   (let [dump (read-dump! dump-path)
         catalog (read-catalog! (str (io/file output-dir "catalog.jsonl")))
         bindings (read-json! (str (io/file output-dir "bindings.json")))
         index (catalog-index! catalog)
         _ (validate-source! dump bindings index)
         entries (vals index)
         pending (count (filter #(nil? (get % "target")) entries))
         translated (- (count entries) pending)]
     {:ok true :slots (get bindings "slots") :units (count entries)
      :translated translated :pending pending})))

(defn apply!
  ([] (apply! default-dump default-output-dir default-applied))
  ([dump-path output-dir output-path]
   (let [dump (read-dump! dump-path)
         catalog (read-catalog! (str (io/file output-dir "catalog.jsonl")))
         bindings (read-json! (str (io/file output-dir "bindings.json")))
         index (catalog-index! catalog)
         _ (validate-source! dump bindings index)
         slot-bindings (get bindings "bindings")
         output
         (ordered-map
          (for [[category entries] dump]
            [category
             (ordered-map
              (for [[id source] entries]
                (if (boolean (re-matches #"(?s)[\s\u200B\uFEFF\u3000]*" source))
                  [id source]
                  (let [binding (get-in slot-bindings [category id])
                        entry (get index (get binding "unit"))]
                    [id (expand-unit index entry (get binding "args" {}) true)]))))]))]
     (atomic-write-json! output-path output true)
     {:output output-path :slots (get bindings "slots") :source-digest (dump-digest dump)})))

(defn- fill-targets-with-source! [catalog-path]
  (let [catalog (read-catalog! catalog-path)
        header (assoc (dissoc catalog "entries") "record_type" "header")
        filled (map #(assoc % "target" (get % "source"))
                    (get catalog "entries"))]
    (atomic-write-jsonl! catalog-path (cons header filled))))

(defn selftest! []
  (let [root (.toFile (Files/createTempDirectory "sst-translation-template-test-"
                                                 (make-array java.nio.file.attribute.FileAttribute 0)))
        dump-path (str (io/file root "dump.json"))
        out-dir (str (io/file root "out"))
        applied (str (io/file root "applied.json"))
        dump
        {"mlCharacterText_Name" {"1" "櫻木 真乃" "2" "風野 灯織"}
         "mlCharacterText_FirstName" {"1" "真乃" "2" "灯織"}
         "mlCharacterText_LastName" {"1" "櫻木" "2" "風野"}
         "rank" {"1" "「春祭り」イベントptランキング10位入賞"
                 "2" "「夏祭り」イベントptランキング20位入賞"}
         "character" {"1" "櫻木 真乃のVo100UP" "2" "風野 灯織のDa200UP"}
         "card" {"1" "【春のカード】" "2" "【夏のカード】"}
         "original" {"1" "第{0}話 %s" "2" "第{0}話 %s"}
         "tag" {"1" "<color=#FF3300>10</color>獲得"
                "2" "<color=#FF3300>20</color>獲得"}
         "blank" {"0" ""}}]
    (try
      (atomic-write-json! dump-path dump true)
      (let [generated (generate! dump-path out-dir)]
        (assert (pos? (:entities generated)))
        (assert (pos? (:templates generated))))
      (fill-targets-with-source! (str (io/file out-dir "catalog.jsonl")))
      ;; Regeneration must preserve completed targets without loading the whole
      ;; catalog into memory.
      (generate! dump-path out-dir)
      (let [checked (check! dump-path out-dir)]
        (assert (:ok checked))
        (assert (zero? (:pending checked))))
      (apply! dump-path out-dir applied)
      (assert (= (canonical-dump dump) (canonical-dump (read-dump! applied))))
      true
      (finally
        (fs/delete-tree root)))))
